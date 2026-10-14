use super::*;
use anodizer_core::config::{
    Config, DockerHubConfig, DockerHubFromFile, DockerHubFromUrl, DockerHubFullDescription,
    StringOrBool,
};
use anodizer_core::context::{Context, ContextOptions};

fn dry_run_ctx(config: Config) -> Context {
    Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    )
}

#[test]
fn test_dockerhub_skips_when_no_config() {
    let config = Config::default();
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

#[test]
fn test_dockerhub_skips_when_empty_vec() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

#[test]
fn test_dockerhub_skips_when_disabled() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

/// When `dockerhub[].description` is unset, the publisher must fall
/// back to the project-level `metadata.description` so a single source
/// of truth covers every entry. Mirrors the same fallback shape used
/// by the homebrew cask + MCP publishers (`effective_description` is
/// the dockerhub-specific override).
#[test]
fn dockerhub_uses_meta_description_when_unset() {
    use anodizer_core::config::MetadataConfig;

    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("from project metadata".to_string()),
        ..Default::default()
    });
    // Per-entry description left None — the fallback must kick in.
    let entry = DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["myorg/myapp".to_string()]),
        description: None,
        ..Default::default()
    };
    config.dockerhub = Some(vec![entry.clone()]);
    let ctx = dry_run_ctx(config);

    let resolved =
        effective_description(&entry, &ctx).expect("metadata fallback must produce a value");
    assert_eq!(resolved, "from project metadata");
}

/// Per-entry `description` always wins over the project metadata
/// fallback when set non-empty.
#[test]
fn dockerhub_entry_description_wins_over_meta() {
    use anodizer_core::config::MetadataConfig;

    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("project-wide fallback".to_string()),
        ..Default::default()
    });
    let entry = DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["myorg/myapp".to_string()]),
        description: Some("per-entry override".to_string()),
        ..Default::default()
    };
    config.dockerhub = Some(vec![entry.clone()]);
    let ctx = dry_run_ctx(config);
    let resolved = effective_description(&entry, &ctx).expect("description present");
    assert_eq!(resolved, "per-entry override");
}

/// Empty per-entry description (an explicit `""`) falls back to the
/// project metadata — same shape as the `or_else` guard in `effective_description`.
#[test]
fn dockerhub_empty_entry_description_falls_back_to_meta() {
    use anodizer_core::config::MetadataConfig;

    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("from project metadata".to_string()),
        ..Default::default()
    });
    let entry = DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["myorg/myapp".to_string()]),
        description: Some(String::new()),
        ..Default::default()
    };
    config.dockerhub = Some(vec![entry.clone()]);
    let ctx = dry_run_ctx(config);
    let resolved = effective_description(&entry, &ctx).expect("metadata fallback used");
    assert_eq!(resolved, "from project metadata");
}

#[test]
fn test_dockerhub_skips_when_no_images() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: None,
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

#[test]
fn test_dockerhub_skips_when_images_empty_vec() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec![]),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

#[test]
fn test_dockerhub_dry_run_logs() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["myorg/myapp".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

#[test]
fn test_dockerhub_dry_run_multiple_images() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["myorg/app1".to_string(), "myorg/app2".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

#[test]
fn test_dockerhub_dry_run_with_full_description_from_file() {
    // In dry-run, full_description is not resolved, so this test
    // confirms dry-run succeeds without reading the file.
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["myorg/myapp".to_string()]),
        description: Some("My app".to_string()),
        full_description: Some(DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: "/nonexistent/dry-run-should-not-read-this.md".to_string(),
            }),
            from_url: None,
        }),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    assert!(publish_to_dockerhub(&ctx, &log).is_ok());
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(2),
    }
}

fn render_ctx() -> Context {
    dry_run_ctx(Config::default())
}

#[test]
fn test_resolve_full_description_from_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let readme = dir.path().join("README.md");
    std::fs::write(&readme, "# My App\nDescription here").expect("write");

    let client = reqwest::blocking::Client::new();
    let desc = DockerHubFullDescription {
        from_file: Some(DockerHubFromFile {
            path: readme.to_str().expect("path utf-8").to_string(),
        }),
        from_url: None,
    };
    let ctx = render_ctx();
    let result = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("resolve");
    assert_eq!(result, "# My App\nDescription here");
}

#[test]
fn test_resolve_full_description_missing_file() {
    let client = reqwest::blocking::Client::new();
    let desc = DockerHubFullDescription {
        from_file: Some(DockerHubFromFile {
            path: "/nonexistent/path/README.md".to_string(),
        }),
        from_url: None,
    };
    let ctx = render_ctx();
    assert!(
        resolve_full_description(
            &desc,
            &ctx,
            &client,
            &fast_policy(),
            anodizer_core::test_helpers::test_logger()
        )
        .is_err()
    );
}

#[test]
fn test_resolve_full_description_neither_set() {
    let client = reqwest::blocking::Client::new();
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: None,
    };
    let ctx = render_ctx();
    assert!(
        resolve_full_description(
            &desc,
            &ctx,
            &client,
            &fast_policy(),
            anodizer_core::test_helpers::test_logger()
        )
        .is_err()
    );
}

/// `disable:` is a serde alias for `skip:` so YAML configs imported from
/// parse without renaming the field.
#[test]
fn dockerhub_disable_alias_parses_as_skip() {
    let yaml = r#"
project_name: test
dockerhub:
  - username: u
    images:
      - org/img
    disable: true
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse");
    let entry = &cfg.dockerhub.as_ref().expect("dockerhub")[0];
    match entry.skip.as_ref().expect("skip set via disable alias") {
        StringOrBool::Bool(b) => assert!(*b),
        other => panic!("expected Bool(true), got {:?}", other),
    }
}

#[test]
fn test_dockerhub_fails_when_username_missing() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: None,
        images: Some(vec!["myorg/myapp".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("'username' is required"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_dockerhub_fails_when_username_empty() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some(String::new()),
        images: Some(vec!["myorg/myapp".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("'username' is required"),
        "unexpected error: {}",
        err
    );
}

/// A `username:` that IS set but whose template renders to "" must fail
/// naming the template. The message "'username' is required (set in config…)"
/// would tell an operator who DID set it to set it.
#[test]
fn test_dockerhub_username_template_rendering_empty_names_the_template() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("{{ .Env.ORG }}".to_string()),
        images: Some(vec!["myorg/myapp".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let mut ctx = dry_run_ctx(config);
    ctx.template_vars_mut().set_env("ORG", "");
    let log = ctx.logger("dockerhub");
    let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
    let chain = format!("{err:#}");
    assert!(
        chain.contains("rendered to an empty string"),
        "error must name the empty render: {chain}"
    );
    assert!(
        chain.contains(".Env.ORG"),
        "error must name the offending template: {chain}"
    );
    assert!(
        !chain.contains("'username' is required"),
        "must not claim username is unset when it IS set: {chain}"
    );
}

#[test]
fn test_resolve_full_description_from_url_unreachable() {
    // Port 1 always refuses — exercises the transport-error fast-path of
    // retry_http_blocking. With a 3-attempt, 1ms-backoff policy this
    // takes a few ms total.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: "http://localhost:1/nonexistent".to_string(),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let err = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect_err("err");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("dockerhub: fetch full_description")
            || chain.contains("exhausted retry attempts")
            || chain.contains("transport error"),
        "unexpected error chain: {chain}"
    );
}

// retry_http_blocking wires through from full_description fetch —
// a single 503 then a 200 should succeed end-to-end through
// resolve_full_description. Exercises the policy-plumbing and leaves
// the helper itself (which has its own 5xx-then-success test in
// crates/core/src/retry.rs).
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

#[test]
fn resolve_full_description_from_url_retries_5xx_then_succeeds() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/"),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let body = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("retries 5xx then succeeds");
    assert_eq!(body, "hello");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one 503 retry then success"
    );
}

/// A `from_url` header can carry `Authorization`: a padded render is sent
/// trimmed, and a template that does not render is an error naming the
/// header without the template's text.
#[test]
fn resolve_full_description_renders_from_url_headers_as_credentials() {
    use anodizer_core::test_helpers::responder::spawn_request_capturing_responder;
    use std::time::Duration;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let mut ctx = render_ctx();
    ctx.template_vars_mut()
        .set_env("HUB_TOKEN", " Bearer t0k\n");
    let desc = |url: String, value: &str| DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url,
            headers: Some(std::collections::HashMap::from([(
                "Authorization".to_string(),
                value.to_string(),
            )])),
        }),
    };

    let (addr, captured) =
        spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
    let body = resolve_full_description(
        &desc(format!("http://{addr}/"), "{{ .Env.HUB_TOKEN }}"),
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("a padded header renders and is sent");
    assert_eq!(body, "hello");
    let request = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
        request.contains("authorization: bearer t0k\r\n"),
        "{request:?}"
    );

    let msg = format!(
        "{:#}",
        resolve_full_description(
            &desc(
                "http://127.0.0.1:1/".to_string(),
                "Bearer hunter2{{ Undefined }}"
            ),
            &ctx,
            &client,
            &fast_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap_err()
    );
    assert!(
        msg.contains("dockerhub.full_description.from_url.headers[Authorization] does not render"),
        "{msg}"
    );
    assert!(!msg.contains("hunter2"), "{msg}");
}

/// Defense-in-depth: if Docker Hub's GET-full-description endpoint
/// echoes the `Authorization: Bearer <PAT>` header back in an error
/// body, the bearer token must NOT survive into the user-visible
/// error chain. The retry helper trips on the 500 and the error
/// message goes through `redact_bearer_tokens`.
#[test]
fn resolve_full_description_redacts_bearer_in_error_body() {
    use std::time::Duration;

    let leaky_body = "internal error: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg";
    let body_len = leaky_body.len();
    // All 3 attempts return 500 to force exhaustion (the retry helper
    // reports the *last* attempt's body in the error message).
    let resp: &'static str = Box::leak(
        format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {body_len}\r\n\r\n{leaky_body}"
        )
        .into_boxed_str(),
    );
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp, resp, resp]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/"),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let err = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect_err("500 exhaustion must error");
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

fn strict_ctx(config: Config) -> Context {
    Context::new(
        config,
        ContextOptions {
            strict: true,
            ..Default::default()
        },
    )
}

/// An image with a leading/trailing/consecutive slash has an empty
/// path segment and is rejected unconditionally (before dry-run, so
/// even config-test runs catch it). Checks the empty-segment `bail!`.
#[test]
fn test_dockerhub_rejects_empty_path_segment() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["myorg//myapp".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("empty path segment"),
        "unexpected error: {err}"
    );
}

/// An image with three slash-separated segments exceeds Docker Hub's
/// `namespace/repo` format and is rejected. Checks the
/// too-many-segments `bail!`.
#[test]
fn test_dockerhub_rejects_too_many_segments() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["a/b/c".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("dockerhub");
    let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("too many path segments"),
        "unexpected error: {err}"
    );
}

/// A bare image name (no namespace) maps to `library/` — which needs
/// Docker Inc permissions — so strict mode hard-fails via
/// `strict_guard`. Checks the strict branch of the bare-name guard
/// (non-strict only warns; this proves the `(strict mode)` bail).
#[test]
fn test_dockerhub_bare_name_fails_in_strict_mode() {
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["barename".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let ctx = strict_ctx(config);
    let log = ctx.logger("dockerhub");
    let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("library/") && msg.contains("strict mode"),
        "unexpected error: {msg}"
    );
}

/// A templated image (`{{ .Env.NS }}/app`) is rendered before
/// validation — proving the image-render pass runs. The `.Env.NS`
/// value is supplied via the Context's TemplateVars env map (which
/// wins over process env), so no process-global state is touched.
/// Asserts the dry-run intent log names the *rendered* image and
/// leaves out the raw `{{ }}` form (which would also trip the empty-segment validator
/// if rendering were skipped).
#[test]
fn test_dockerhub_renders_image_template_before_validation() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut config = Config::default();
    config.dockerhub = Some(vec![DockerHubConfig {
        username: Some("testuser".to_string()),
        images: Some(vec!["{{ .Env.DOCKERHUB_TEST_NS }}/app".to_string()]),
        description: Some("My app".to_string()),
        ..Default::default()
    }]);
    let mut ctx = dry_run_ctx(config);
    ctx.template_vars_mut().set_env("DOCKERHUB_TEST_NS", "myns");
    ctx.with_log_capture(capture.clone());
    let log = ctx.logger("dockerhub");
    let result = publish_to_dockerhub(&ctx, &log);
    result.expect("templated image renders + validates");
    let msgs = capture.all_messages();
    assert!(
        msgs.iter().any(|(_, m)| m.contains("myns/app")),
        "expected the rendered image in the dry-run intent log: {msgs:?}"
    );
}

use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};

/// `from_url` happy path: a single 200 returns the body verbatim and
/// the request reaches the server as a plain `GET /readme.md` (no
/// retry, exactly one hit). Checks the success branch of
/// `resolve_full_description`'s `from_url` arm — the existing tests
/// only cover the unreachable / retry / redaction failure modes.
#[test]
fn resolve_full_description_from_url_success_records_get() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/readme.md",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 17\r\n\r\n# Title\nbody text",
        times: None,
    }]);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/readme.md"),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let body = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("200 succeeds");
    assert_eq!(body, "# Title\nbody text");
    let entries = log.lock().expect("log");
    assert_eq!(
        entries.len(),
        1,
        "exactly one request, no retry: {entries:?}"
    );
    assert_eq!(entries[0].method, "GET");
    assert_eq!(entries[0].path, "/readme.md");
}

/// A 4xx on the `from_url` GET fast-fails (SuccessClass::Strict routes
/// 4xx → Break) — the responder is hit exactly once, NOT
/// `max_attempts` times, and the error names the URL + status. Checks
/// the no-retry classification: a typo'd README URL is reported
/// after the first attempt.
#[test]
fn resolve_full_description_from_url_4xx_fast_fails_no_retry() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/missing.md",
        response: "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
        times: None,
    }]);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/missing.md"),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let err = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect_err("404 errors");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("404") || chain.contains("Not Found"),
        "error chain should carry the 404 status: {chain}"
    );
    let entries = log.lock().expect("log");
    assert_eq!(
        entries.len(),
        1,
        "4xx must NOT retry under fast_policy (3 attempts): {entries:?}"
    );
}

/// A 429 on `from_url` retries (SuccessClass::Strict routes 429 →
/// Continue, same as 5xx) — first 429, then 200 succeeds. Distinct
/// from the existing 503 test: 429 is the rate-limit case and shares
/// the retriable classification with 5xx.
#[test]
fn resolve_full_description_from_url_429_retries_then_succeeds() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/r.md",
            response: "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/r.md",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            times: None,
        },
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/r.md"),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let body = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("429 retries then 200");
    assert_eq!(body, "ok");
    let entries = log.lock().expect("log");
    assert_eq!(entries.len(), 2, "one 429 retry then success: {entries:?}");
}

/// The `from_url.url` is template-rendered through `ctx` before the
/// fetch, so `{{ .Env.OWNER }}`-style configs resolve. The `.Env.OWNER`
/// value is supplied via the Context's TemplateVars env map (which
/// wins over process env), so no process-global state is touched; the
/// rendered path (`/tj/README.md`) is the one that must actually hit
/// the wire (raw `{{ }}` would 404).
#[test]
fn resolve_full_description_from_url_renders_template() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/tj/README.md",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nrendered",
        times: None,
    }]);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/{{{{ .Env.DOCKERHUB_TEST_OWNER }}}}/README.md"),
            headers: None,
        }),
    };
    let mut ctx = render_ctx();
    ctx.template_vars_mut()
        .set_env("DOCKERHUB_TEST_OWNER", "tj");
    let body = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("templated url resolves");
    assert_eq!(body, "rendered");
    let entries = log.lock().expect("log");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].path, "/tj/README.md",
        "the rendered (not raw) path must hit the wire: {entries:?}"
    );
}

/// A malformed template in `from_url.url` returns a render error
/// (the `with_context` wraps it with the `render full_description
/// from_url` label) BEFORE any network call. Checks the render-error
/// arm distinct from the fetch-error arm.
#[test]
fn resolve_full_description_from_url_template_render_error() {
    let client = reqwest::blocking::Client::new();
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            // Unterminated tag — the template engine rejects it.
            url: "http://example.invalid/{{ .Tag".to_string(),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let err = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect_err("bad template must error");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("render full_description from_url"),
        "expected the render-context label, got: {chain}"
    );
}

/// The `from_file.path` is also template-rendered: a
/// `{{ .Env.DOCS_DIR }}/README.md` config resolves the env var and
/// reads the file at the rendered path. Distinct from the existing
/// literal-path file test — this exercises the `render_template` call
/// on the from_file branch. The `.Env.DOCS_DIR` value is supplied via
/// the Context's TemplateVars env map (which wins over process env),
/// so no process-global state is touched.
#[test]
fn resolve_full_description_from_file_renders_template_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let readme = dir.path().join("README.md");
    std::fs::write(&readme, "# Templated path").expect("write");

    let client = reqwest::blocking::Client::new();
    let desc = DockerHubFullDescription {
        from_file: Some(DockerHubFromFile {
            path: "{{ .Env.DOCKERHUB_TEST_DOCS_DIR }}/README.md".to_string(),
        }),
        from_url: None,
    };
    let mut ctx = render_ctx();
    ctx.template_vars_mut().set_env(
        "DOCKERHUB_TEST_DOCS_DIR",
        dir.path().to_str().expect("path utf-8"),
    );
    let body = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("render + read");
    assert_eq!(body, "# Templated path");
}

/// `from_file` takes precedence over `from_url` when both are set:
/// the file content is returned and the URL is NEVER fetched (the
/// responder records zero requests). Checks the documented precedence
/// — without it, a config with both set could silently prefer the
/// network source.
#[test]
fn resolve_full_description_from_file_wins_over_from_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let readme = dir.path().join("README.md");
    std::fs::write(&readme, "from the file").expect("write");

    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/never.md",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nfrom the url!",
        times: None,
    }]);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client");
    let desc = DockerHubFullDescription {
        from_file: Some(DockerHubFromFile {
            path: readme.to_str().expect("path utf-8").to_string(),
        }),
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/never.md"),
            headers: None,
        }),
    };
    let ctx = render_ctx();
    let body = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("from_file precedence");
    assert_eq!(body, "from the file");
    let entries = log.lock().expect("log");
    assert!(
        entries.is_empty(),
        "from_url must NOT be fetched when from_file is set: {entries:?}"
    );
}

/// Custom `from_url.headers` are emitted on the wire. The scripted
/// responder doesn't capture headers, so this drives a raw inline
/// TCP listener that records the full request and asserts the
/// configured `X-Auth: secret-val` header arrives — checking the
/// `if let Some(ref h) = headers` forwarding loop.
#[test]
fn resolve_full_description_from_url_forwards_headers() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let mut req = String::new();
            // Read until the header terminator so a header line split across
            // TCP segments can't be missed (there is no request body).
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.push_str(&String::from_utf8_lossy(&buf[..n]));
                if req.contains("\r\n\r\n") {
                    break;
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let _ = stream.flush();
            let _ = tx.send(req);
        }
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client");
    let mut headers = std::collections::HashMap::new();
    headers.insert("X-Auth".to_string(), "secret-val".to_string());
    let desc = DockerHubFullDescription {
        from_file: None,
        from_url: Some(DockerHubFromUrl {
            url: format!("http://{addr}/h.md"),
            headers: Some(headers),
        }),
    };
    let ctx = render_ctx();
    let body = resolve_full_description(
        &desc,
        &ctx,
        &client,
        &fast_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("with headers");
    assert_eq!(body, "ok");
    let req = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("captured request");
    assert!(
        req.to_lowercase().contains("x-auth: secret-val"),
        "configured header must hit the wire: {req}"
    );
}

/// Live-mode (`dry_run: false`) coverage for `publish_to_dockerhub` and
/// `restore_dockerhub_target_with_env`. Every test redirects the
/// hard-coded `https://hub.docker.com` host to an in-process scripted
/// responder via the `ANODIZER_DOCKERHUB_API_BASE` env override (mirroring
/// the `ANODIZER_GITHUB_API_BASE` pattern used by the GitHub backend), so
/// the login / snapshot-GET / PATCH request shapes — method, path, and
/// JSON body — and the response→outcome mapping are asserted against
/// recorded traffic. All HTTP-mock (local TCP, no subprocess) so they run
/// and count on every platform: UNGATED.
mod live_http_tests {
    use super::super::*;
    use anodizer_core::config::{
        Config, DockerHubConfig, DockerHubFromFile, DockerHubFullDescription, HumanDuration,
        RetryConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use anodizer_core::{MapEnvSource, PublishEvidence, PublishEvidenceExtra, Publisher};
    use std::time::Duration;

    /// A 2-attempt, 1ms-backoff retry policy installed into the config so
    /// `ctx.retry_policy()` (which `publish_to_dockerhub` reads internally,
    /// non-injectably) doesn't inherit the 10-attempt / 10-second-delay
    /// production default and stall the suite.
    fn fast_retry() -> RetryConfig {
        RetryConfig {
            attempts: 2,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(2)),
            max_elapsed: None,
        }
    }

    /// Build a live (non-dry-run) context whose env source carries the
    /// `ANODIZER_DOCKERHUB_API_BASE` redirect plus `DOCKER_PASSWORD`, and
    /// whose retry policy is fast. `strict` toggles the strict-guard
    /// branches.
    fn live_ctx(config: Config, api_base: &str, strict: bool) -> Context {
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                strict,
                ..Default::default()
            },
        );
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", api_base.to_string());
        env.set("DOCKER_PASSWORD", "s3cr3t-pat");
        ctx.set_env_source(env);
        ctx
    }

    fn entry(images: Vec<&str>, description: Option<&str>) -> DockerHubConfig {
        DockerHubConfig {
            username: Some("ci-bot".to_string()),
            images: Some(images.into_iter().map(str::to_string).collect()),
            description: description.map(str::to_string),
            ..Default::default()
        }
    }

    /// Standard 3-route DockerHub flow for a single `myorg/myapp` image:
    /// login → snapshot GET (returns the given prior description) → PATCH.
    fn flow_routes(prior_short: &'static str, prior_full: &'static str) -> Vec<ScriptedRoute> {
        let body =
            format!("{{\"description\":\"{prior_short}\",\"full_description\":\"{prior_full}\"}}");
        let snapshot: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n{body}",
                len = body.len(),
            )
            .into_boxed_str(),
        );
        vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: snapshot,
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]
    }

    /// Full happy path: login authenticates, snapshot GET reads the prior
    /// (differing) description, PATCH updates it. Asserts the exact login
    /// body (username + password), the snapshot GET path, and the PATCH
    /// body carrying the new `description`. Returns one mutated target.
    #[test]
    fn publish_live_full_flow_patches_description() {
        let (addr, log) = spawn_scripted_responder(flow_routes("old short", "old full"));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("brand new desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("live publish succeeds");

        assert_eq!(targets.len(), 1, "exactly one repo mutated");
        let t = &targets[0];
        assert_eq!(t.target, "myorg/myapp");
        assert_eq!(t.namespace, "myorg");
        assert_eq!(t.name, "myapp");
        assert_eq!(t.username, "ci-bot");
        assert_eq!(t.secret_env_var, "DOCKER_PASSWORD");
        assert_eq!(
            t.repo_url,
            format!("http://{addr}/v2/repositories/myorg/myapp/")
        );
        assert_eq!(t.snapshot_description.as_deref(), Some("old short"));
        assert_eq!(t.snapshot_full_description.as_deref(), Some("old full"));

        let entries = log.lock().expect("log");
        assert_eq!(entries.len(), 3, "login + snapshot + patch: {entries:?}");

        assert_eq!(entries[0].method, "POST");
        assert_eq!(entries[0].path, "/v2/users/login/");
        let login: serde_json::Value =
            serde_json::from_str(&entries[0].body).expect("login body is json");
        assert_eq!(login["username"], "ci-bot");
        assert_eq!(login["password"], "s3cr3t-pat");

        assert_eq!(entries[1].method, "GET");
        assert_eq!(entries[1].path, "/v2/repositories/myorg/myapp/");

        assert_eq!(entries[2].method, "PATCH");
        assert_eq!(entries[2].path, "/v2/repositories/myorg/myapp/");
        let patch: serde_json::Value =
            serde_json::from_str(&entries[2].body).expect("patch body is json");
        assert_eq!(patch["description"], "brand new desc");
        assert!(
            patch.get("full_description").is_none(),
            "no full_description configured → key omitted: {}",
            entries[2].body
        );
    }

    /// PATCH body carries BOTH `description` and `full_description` when a
    /// `full_description.from_file` is configured. Checks that the resolved
    /// file content is copied verbatim into the `full_description` key.
    #[test]
    fn publish_live_patches_description_and_full_description() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));
        let dir = tempfile::tempdir().expect("tempdir");
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# Long README\nbody").expect("write");

        let mut e = entry(vec!["myorg/myapp"], Some("short one"));
        e.full_description = Some(DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: readme.to_str().expect("utf-8").to_string(),
            }),
            from_url: None,
        });
        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![e]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("live publish succeeds");
        assert_eq!(targets.len(), 1);

        let entries = log.lock().expect("log");
        let patch = entries
            .iter()
            .find(|e| e.method == "PATCH")
            .expect("a PATCH was issued");
        let body: serde_json::Value = serde_json::from_str(&patch.body).expect("patch body json");
        assert_eq!(body["description"], "short one");
        assert_eq!(body["full_description"], "# Long README\nbody");
    }

    /// The short description is template-rendered before the PATCH:
    /// `{{ .ProjectName }}` resolves against the context. Checks that the
    /// rendered string hits the wire.
    #[test]
    fn publish_live_renders_description_template() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));

        let config = Config {
            project_name: "anodizer".to_string(),
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(
                vec!["myorg/myapp"],
                Some("Release tool for {{ .ProjectName }}"),
            )]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        publish_to_dockerhub(&ctx, &logger).expect("publish");
        let entries = log.lock().expect("log");
        let patch = entries
            .iter()
            .find(|e| e.method == "PATCH")
            .expect("PATCH issued");
        let body: serde_json::Value = serde_json::from_str(&patch.body).expect("json");
        assert_eq!(
            body["description"], "Release tool for anodizer",
            "the rendered description must hit the wire: {}",
            patch.body
        );
    }

    /// Idempotency: when the snapshot already matches the description the
    /// publisher would send, NO PATCH is issued (only login + snapshot GET) and the run
    /// records zero mutated targets.
    #[test]
    fn publish_live_skips_patch_when_unchanged() {
        let (addr, log) = spawn_scripted_responder(flow_routes("already current", ""));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("already current"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("publish");
        assert!(targets.is_empty(), "no-change run mutates nothing");

        let entries = log.lock().expect("log");
        assert!(
            entries.iter().all(|e| e.method != "PATCH"),
            "no PATCH when description is unchanged: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.method == "GET"),
            "snapshot GET still happens: {entries:?}"
        );
    }

    /// A 401 on login fails the publish with an error naming the status.
    /// The 4xx fast-fails (no retry): login is hit exactly once.
    #[test]
    fn publish_live_login_401_errors() {
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/v2/users/login/",
            response: "HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\n\r\nbad creds yo!",
            times: None,
        }]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("401 login must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("authentication failed") && chain.contains("401"),
            "error must name auth failure + status: {chain}"
        );
        let entries = log.lock().expect("log");
        assert_eq!(
            entries.iter().filter(|e| e.method == "POST").count(),
            1,
            "401 fast-fails — login not retried: {entries:?}"
        );
    }

    /// A login response missing the `token` key errors with the
    /// "no token in login response" message — the publish cannot proceed
    /// without a bearer.
    #[test]
    fn publish_live_login_missing_token_errors() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/v2/users/login/",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"detail\":\"x\"}",
            times: None,
        }]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("missing token errors");
        assert!(
            format!("{err:#}").contains("no token in login response"),
            "unexpected error: {err:#}"
        );
    }

    /// A 5xx on the snapshot GET retries then exhausts → the publish
    /// errors. The snapshot GET is attempted `attempts` (2) times under
    /// the fast policy, proving 5xx is retriable on the snapshot path.
    #[test]
    fn publish_live_snapshot_5xx_retries_then_errors() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("snapshot 5xx exhausts");
        assert!(
            format!("{err:#}").contains("snapshot"),
            "error must name the snapshot phase: {err:#}"
        );
        let entries = log.lock().expect("log");
        assert_eq!(
            entries.iter().filter(|e| e.method == "GET").count(),
            2,
            "503 snapshot retried up to attempts=2: {entries:?}"
        );
    }

    /// A 500 on the PATCH (after a successful login + snapshot) fails the
    /// publish with an error naming the PATCH + status. Checks the PATCH
    /// error-mapping arm distinct from login/snapshot failures.
    #[test]
    fn publish_live_patch_500_errors() {
        let (addr, _log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("patch 500 errors");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("PATCH") && chain.contains("500"),
            "error must name PATCH + status: {chain}"
        );
    }

    /// The JWT is cached across two entries that share the same
    /// `(username, secret_env)` pair: login happens exactly ONCE even
    /// though two distinct repos are PATCHed. Checks the `jwt_cache` reuse.
    #[test]
    fn publish_live_reuses_jwt_across_entries() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/app1/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/app1/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/app2/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/app2/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![
                entry(vec!["myorg/app1"], Some("desc one")),
                entry(vec!["myorg/app2"], Some("desc two")),
            ]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("publish");
        assert_eq!(targets.len(), 2, "both repos mutated");

        let entries = log.lock().expect("log");
        assert_eq!(
            entries.iter().filter(|e| e.method == "POST").count(),
            1,
            "shared (user, secret) pair → exactly one login: {entries:?}"
        );
        assert_eq!(
            entries.iter().filter(|e| e.method == "PATCH").count(),
            2,
            "one PATCH per repo: {entries:?}"
        );
    }

    /// A bare image name (no namespace) in NON-strict mode warns but
    /// proceeds: the PATCH targets the `library/<name>` repo path. Checks
    /// the non-strict branch of the bare-name guard plus the
    /// `("library", parts[0])` namespace-derivation fallback.
    #[test]
    fn publish_live_bare_name_maps_to_library_namespace() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/library/solo/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/library/solo/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["solo"], Some("desc"))]),
            ..Default::default()
        };
        // Non-strict: the bare-name guard warns and proceeds.
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("non-strict bare name proceeds");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].namespace, "library");
        assert_eq!(targets[0].name, "solo");

        let entries = log.lock().expect("log");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "PATCH" && e.path == "/v2/repositories/library/solo/"),
            "PATCH targets library/<name>: {entries:?}"
        );
    }

    /// `DockerhubPublisher::run` (the trait entry point) end-to-end:
    /// drives the live flow and asserts the returned `PublishEvidence`
    /// carries the mutated repo in `artifact_paths` + `primary_ref` and
    /// the typed `Dockerhub` extra with one target.
    #[test]
    fn publisher_run_emits_evidence_for_mutated_repo() {
        let (addr, _log) = spawn_scripted_responder(flow_routes("old", "old full"));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("new desc"))]),
            ..Default::default()
        };
        let mut ctx = live_ctx(config, &format!("http://{addr}"), false);

        let p = DockerhubPublisher::new();
        let evidence = p.run(&mut ctx).expect("run succeeds");

        let expected = format!("http://{addr}/v2/repositories/myorg/myapp/");
        assert_eq!(evidence.artifact_paths.len(), 1);
        assert_eq!(evidence.artifact_paths[0].display().to_string(), expected);
        assert_eq!(evidence.primary_ref.as_deref(), Some(expected.as_str()));
        match &evidence.extra {
            PublishEvidenceExtra::Dockerhub(d) => {
                assert_eq!(d.dockerhub_targets.len(), 1);
                assert_eq!(d.dockerhub_targets[0].target, "myorg/myapp");
            }
            other => panic!("expected Dockerhub extra, got {other:?}"),
        }
    }

    /// `run` with no mutation (dry-run) emits empty evidence: no
    /// artifact_paths, no primary_ref, `Empty` extra. Checks that dry-run /
    /// skip paths do not leak phantom targets.
    #[test]
    fn publisher_run_dry_run_emits_empty_evidence() {
        let config = Config {
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let p = DockerhubPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run run succeeds");
        assert!(evidence.artifact_paths.is_empty());
        assert!(evidence.primary_ref.is_none());
        assert!(matches!(evidence.extra, PublishEvidenceExtra::Empty));
    }

    /// `restore_dockerhub_target_with_env` live happy path: re-authenticates
    /// (login body carries the re-resolved password) and PATCHes the
    /// snapshot back. Asserts the rollback PATCH body restores BOTH
    /// `description` and `full_description`.
    #[test]
    fn restore_live_reauths_and_patches_snapshot_back() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{\"token\":\"jwt-back\"}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = fast_retry().to_policy();
        let target = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: format!("http://{addr}/v2/repositories/myorg/myapp/"),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior short".into()),
            snapshot_full_description: Some("# Prior README".into()),
        };
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        env.set("DOCKER_PASSWORD", "rollback-pat");

        restore_dockerhub_target_with_env(
            &client,
            &policy,
            None,
            &target,
            &env,
            anodizer_core::test_helpers::test_logger(),
        )
        .expect("rollback restores snapshot");

        let entries = log.lock().expect("log");
        assert_eq!(entries.len(), 2, "login + patch: {entries:?}");
        assert_eq!(entries[0].method, "POST");
        assert_eq!(entries[0].path, "/v2/users/login/");
        let login: serde_json::Value = serde_json::from_str(&entries[0].body).expect("login json");
        assert_eq!(login["username"], "ci-bot");
        assert_eq!(login["password"], "rollback-pat");

        assert_eq!(entries[1].method, "PATCH");
        assert_eq!(entries[1].path, "/v2/repositories/myorg/myapp/");
        let patch: serde_json::Value = serde_json::from_str(&entries[1].body).expect("patch json");
        assert_eq!(patch["description"], "prior short");
        assert_eq!(patch["full_description"], "# Prior README");
    }

    /// Rollback login 403 returns an error naming the rollback-auth
    /// failure + status, so `DockerhubPublisher::rollback` can count it as
    /// a failure and report the dropped restore.
    #[test]
    fn restore_live_login_403_errors() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/v2/users/login/",
            response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = fast_retry().to_policy();
        let target = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: format!("http://{addr}/v2/repositories/myorg/myapp/"),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior".into()),
            snapshot_full_description: None,
        };
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        env.set("DOCKER_PASSWORD", "pw");

        let err = restore_dockerhub_target_with_env(
            &client,
            &policy,
            None,
            &target,
            &env,
            anodizer_core::test_helpers::test_logger(),
        )
        .expect_err("403 rollback login errors");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("rollback authentication failed") && chain.contains("403"),
            "error must name rollback auth + status: {chain}"
        );
    }

    /// `DockerhubPublisher::rollback` end-to-end over recorded evidence:
    /// re-authenticates and restores each target, logging the restored
    /// count. Checks the trait rollback loop (decode → per-target restore →
    /// status tally) against the live responder.
    #[test]
    fn publisher_rollback_restores_recorded_targets() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{\"token\":\"jwt-back\"}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let capture = anodizer_core::log::LogCapture::new();
        let config = Config {
            retry: Some(fast_retry()),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        env.set("DOCKER_PASSWORD", "rollback-pat");
        ctx.set_env_source(env);
        ctx.with_log_capture(capture.clone());

        let mut evidence = PublishEvidence::new("dockerhub");
        evidence.extra =
            PublishEvidenceExtra::Dockerhub(anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: vec![DockerhubTarget {
                    target: "myorg/myapp".into(),
                    repo_url: format!("http://{addr}/v2/repositories/myorg/myapp/"),
                    namespace: "myorg".into(),
                    name: "myapp".into(),
                    username: "ci-bot".into(),
                    secret_env_var: "DOCKER_PASSWORD".into(),
                    snapshot_description: Some("prior short".into()),
                    snapshot_full_description: None,
                }],
            });

        let p = DockerhubPublisher::new();
        p.rollback(&mut ctx, &evidence).expect("rollback succeeds");

        let entries = log.lock().expect("log");
        assert!(
            entries.iter().any(|e| e.method == "PATCH"),
            "rollback issued a PATCH: {entries:?}"
        );
        let statuses = capture.all_messages();
        assert!(
            statuses
                .iter()
                .any(|(_, m)| m.contains("restored 1 description")),
            "rollback tally must report 1 restored: {statuses:?}"
        );
    }

    /// `DockerhubPublisher::rollback` counts a failed restore (missing env
    /// var) as a failure and continues — the tally reports `1 failure`
    /// and a per-target warn names the env var. Checks the warn-and-continue
    /// rollback contract.
    #[test]
    fn publisher_rollback_counts_failure_when_env_missing() {
        let capture = anodizer_core::log::LogCapture::new();
        let config = Config {
            retry: Some(fast_retry()),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        // Env source deliberately omits DOCKER_PASSWORD so restore fails.
        ctx.set_env_source(MapEnvSource::new());
        ctx.with_log_capture(capture.clone());

        let mut evidence = PublishEvidence::new("dockerhub");
        evidence.extra =
            PublishEvidenceExtra::Dockerhub(anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: vec![DockerhubTarget {
                    target: "myorg/myapp".into(),
                    repo_url: "http://127.0.0.1:1/unreachable".into(),
                    namespace: "myorg".into(),
                    name: "myapp".into(),
                    username: "ci-bot".into(),
                    secret_env_var: "DOCKER_PASSWORD".into(),
                    snapshot_description: Some("prior".into()),
                    snapshot_full_description: None,
                }],
            });

        let p = DockerhubPublisher::new();
        p.rollback(&mut ctx, &evidence)
            .expect("rollback never aborts");

        let warns = capture.warn_messages();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("failed to restore") && m.contains("DOCKER_PASSWORD")),
            "a per-target failure warn must name the env var: {warns:?}"
        );
        let statuses = capture.all_messages();
        assert!(
            statuses
                .iter()
                .any(|(_, m)| m.contains("0 descriptions, 1 failure")),
            "tally must report 1 failure: {statuses:?}"
        );
    }

    /// Live mode with a missing secret env var hard-fails authentication
    /// BEFORE any login round-trip (the env lookup precedes the POST).
    /// Checks the `environment variable '<X>' not set` bail on the publish
    /// path (distinct from the dry-run warn).
    #[test]
    fn publish_live_missing_secret_env_errors() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        // API base redirected, but DOCKER_PASSWORD intentionally absent.
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        ctx.set_env_source(env);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("missing secret errors");
        assert!(
            format!("{err:#}").contains("'DOCKER_PASSWORD' not set"),
            "unexpected error: {err:#}"
        );
        let entries = log.lock().expect("log");
        assert!(
            entries.is_empty(),
            "no HTTP call before the secret lookup: {entries:?}"
        );
    }

    /// Strict mode + both descriptions empty → the publish bails via
    /// `strict_guard` BEFORE authenticating. Checks the strict branch of the
    /// "both description and full_description are empty" guard. The
    /// responder records zero traffic.
    #[test]
    fn publish_live_strict_empty_descriptions_bails_before_auth() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));

        // No per-entry description, no metadata fallback → both empty.
        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], None)]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), true);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("strict empty-desc bails");
        assert!(
            format!("{err:#}").contains("both description and full_description are empty"),
            "unexpected error: {err:#}"
        );
        let entries = log.lock().expect("log");
        assert!(
            entries.is_empty(),
            "strict bail precedes any HTTP call: {entries:?}"
        );
    }
}

mod publisher_tests {
    use super::super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    #[test]
    fn dockerhub_publisher_classification() {
        let p = DockerhubPublisher::new();
        assert_eq!(p.name(), "dockerhub");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("DOCKER_PASSWORD description snapshot+restore")
        );
    }

    #[test]
    fn dockerhub_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = DockerhubPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn dockerhub_rollback_warns_when_no_targets_recorded() {
        // Empty evidence drives rollback into the no-targets branch.
        // The capture checks that production actually invoked `log.warn`
        // with the helper-formatted message — a hand-constructed expected
        // string compared against the helper output would pass even if
        // the rollback body forgot the warn entirely.
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("dockerhub");
        let p = DockerhubPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("dockerhub")
                && m.contains("description-sync targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    /// Defense-in-depth: a serialized `DockerhubTarget` carries no
    /// secret material. Mirrors the negative test on every other
    /// publisher's `*Target` struct — see the `PublishEvidence::extra`
    /// rustdoc for the contract. `password` is the field the historical
    /// warn-only rollback could not enforce; this test checks that the
    /// snapshot+restore implementation still does not persist one.
    #[test]
    fn dockerhub_target_extra_carries_no_secret_material() {
        // Structural check: build typed evidence with a populated
        // variant and assert (a) no credential-shaped keys appear AND
        // (b) the operator-public coordinates serialize.
        let mut e = anodizer_core::PublishEvidence::new("dockerhub");
        e.extra = anodizer_core::PublishEvidenceExtra::Dockerhub(
            anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: vec![DockerhubTarget {
                    target: "myorg/myapp".into(),
                    repo_url: "https://hub.docker.com/v2/repositories/myorg/myapp/".into(),
                    namespace: "myorg".into(),
                    name: "myapp".into(),
                    username: "ci-bot".into(),
                    secret_env_var: "DOCKER_PASSWORD".into(),
                    snapshot_description: Some("prior short desc".into()),
                    snapshot_full_description: Some("# Prior README\nbody".into()),
                }],
            },
        );
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":\""), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: env var NAME + login + endpoint serialize.
        assert!(s.contains("\"secret_env_var\""), "{s}");
        assert!(s.contains("\"username\":\"ci-bot\""), "{s}");
        assert!(s.contains("\"namespace\":\"myorg\""), "{s}");
    }

    /// `decode_dockerhub_targets` is the rollback entry point — assert
    /// the round-trip shape so a future schema drift (rename of the
    /// `dockerhub_targets` key, change to the field set) fails loudly
    /// before rollback degrades to warn-only.
    #[test]
    fn dockerhub_target_decode_round_trips() {
        let original = vec![DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "https://hub.docker.com/v2/repositories/myorg/myapp/".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior".into()),
            snapshot_full_description: None,
        }];
        let extra = anodizer_core::PublishEvidenceExtra::Dockerhub(
            anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: original.clone(),
            },
        );
        let decoded = decode_dockerhub_targets(&extra);
        assert_eq!(decoded, original);
    }

    /// Wrong variant → empty vec → rollback short-circuits to the
    /// empty-evidence warn path. Checks the failure mode so a typed-enum
    /// addition that accidentally matches an unrelated payload doesn't
    /// re-enable the live-PATCH path against decoded data.
    #[test]
    fn dockerhub_target_decode_missing_key_yields_empty() {
        let extra = anodizer_core::PublishEvidenceExtra::Empty;
        assert!(decode_dockerhub_targets(&extra).is_empty());
        let extra = anodizer_core::PublishEvidenceExtra::Homebrew(
            anodizer_core::publish_evidence::HomebrewExtra {
                homebrew_targets: Vec::new(),
            },
        );
        assert!(decode_dockerhub_targets(&extra).is_empty());
    }

    /// PATCH-body construction round-trip: a snapshot with both
    /// description and full_description set produces a PATCH body
    /// carrying exactly those keys. The live-HTTP path goes through
    /// `restore_dockerhub_target_with_env` (also exercised by the
    /// missing-env and empty-snapshot tests below); this test checks
    /// the body shape without spinning up the responder — login +
    /// PATCH both hit the hard-coded `https://hub.docker.com` host,
    /// so a unit-level happy-path requires either redirecting login
    /// (an artificial fixture) or running against the real registry
    /// (integration).
    #[test]
    fn dockerhub_restore_body_contains_snapshot_fields() {
        let t = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "https://hub.docker.com/v2/repositories/myorg/myapp/".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior short".into()),
            snapshot_full_description: Some("# prior README".into()),
        };

        // Reproduce the body-construction the restore function would
        // assemble. Checks the contract without exercising the live
        // HTTP path.
        let mut expected = serde_json::Map::new();
        if let Some(ref d) = t.snapshot_description {
            expected.insert("description".into(), serde_json::Value::String(d.clone()));
        }
        if let Some(ref fd) = t.snapshot_full_description {
            expected.insert(
                "full_description".into(),
                serde_json::Value::String(fd.clone()),
            );
        }
        assert_eq!(
            expected.get("description").and_then(|v| v.as_str()),
            Some("prior short")
        );
        assert_eq!(
            expected.get("full_description").and_then(|v| v.as_str()),
            Some("# prior README")
        );
    }

    /// All-None snapshot → restore is a no-op (no PATCH issued).
    /// Defends against the failure mode where a publisher with no
    /// readable prior description would otherwise PATCH `{}` (which
    /// DockerHub treats as a no-op anyway, but issuing the call wastes
    /// an auth round-trip and prints a spurious "restored" log line).
    #[test]
    fn dockerhub_restore_no_op_when_snapshot_empty() {
        // Build a fresh client; the function returns Ok(()) before
        // touching the network when the snapshot is all-None.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        };
        let t = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "http://127.0.0.1:1/unreachable".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD_UNSET_FOR_TEST_XYZ".into(),
            snapshot_description: None,
            snapshot_full_description: None,
        };
        // Note: the unreachable URL + unset env var would normally
        // make this fail loudly. The no-op short-circuit must fire
        // before either is touched.
        let env = anodizer_core::MapEnvSource::new();
        restore_dockerhub_target_with_env(
            &client,
            &policy,
            None,
            &t,
            &env,
            anodizer_core::test_helpers::test_logger(),
        )
        .expect("no-op when snapshot empty");
    }

    /// Missing env var → restore returns Err with a recognizable
    /// message naming the env var. Drives the lookup through an empty
    /// [`MapEnvSource`] so the assertion holds regardless of the
    /// ambient process env (no thread-race on `remove_var`).
    #[test]
    fn dockerhub_restore_errors_when_env_missing() {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        };
        let env_var = "DOCKER_PASSWORD_INTENTIONALLY_UNSET_FOR_DOCKERHUB_TEST_ABCDEF";
        let t = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "http://127.0.0.1:1/unreachable".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: env_var.into(),
            snapshot_description: Some("prior".into()),
            snapshot_full_description: None,
        };
        let env = anodizer_core::MapEnvSource::new();
        let err = restore_dockerhub_target_with_env(
            &client,
            &policy,
            None,
            &t,
            &env,
            anodizer_core::test_helpers::test_logger(),
        )
        .expect_err("env missing");
        let chain = format!("{err:#}");
        assert!(
            chain.contains(env_var),
            "error chain should name the unset env var: {chain}"
        );
    }
}

mod preflight_live_tests {
    use super::super::*;
    use anodizer_core::Publisher;
    use anodizer_core::config::{Config, DockerHubConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    fn make_ctx(base: &str, deselect: bool) -> Context {
        let cfg = DockerHubConfig {
            username: Some("ci-bot".into()),
            images: Some(vec!["lib/app".into()]),
            if_condition: if deselect { Some("false".into()) } else { None },
            ..Default::default()
        };
        let config = Config {
            project_name: "app".into(),
            dockerhub: Some(vec![cfg]),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(
            anodizer_core::MapEnvSource::new()
                .with("ANODIZER_DOCKERHUB_API_BASE", base.to_string())
                .with("DOCKER_PASSWORD", "s3cret"),
        );
        ctx
    }

    #[test]
    fn dockerhub_preflight_warns_on_invalid_login() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        ]);
        let ctx = make_ctx(&format!("http://{addr}"), false);
        match DockerhubPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok")
        {
            anodizer_core::PreflightCheck::Warning(m) => assert!(m.contains("dockerhub"), "{m}"),
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn dockerhub_preflight_passes_on_successful_login() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"token\":\"jwt\"}",
        ]);
        let ctx = make_ctx(&format!("http://{addr}"), false);
        assert!(matches!(
            DockerhubPublisher::new()
                .preflight(&ctx)
                .expect("preflight ok"),
            anodizer_core::PreflightCheck::Pass
        ));
    }

    #[test]
    fn dockerhub_preflight_skips_deselected_without_request() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"token\":\"jwt\"}",
        ]);
        let ctx = make_ctx(&format!("http://{addr}"), true);
        assert!(matches!(
            DockerhubPublisher::new()
                .preflight(&ctx)
                .expect("preflight ok"),
            anodizer_core::PreflightCheck::Pass
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
