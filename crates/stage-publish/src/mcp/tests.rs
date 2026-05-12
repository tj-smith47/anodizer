//! Publisher tests for the MCP registry.
//!
//! Strategy: every test that exercises the publish loop runs against a
//! one-shot HTTP responder bound to an ephemeral port (mirrors the
//! `dockerhub.rs` test harness — we keep the test surface uniform across
//! HTTP publishers). The `auth.token` field is set non-empty so the
//! `NoneAuthProvider::get_token` short-circuit returns the token verbatim
//! without hitting `/v0/auth/none`; the only endpoint a test must serve is
//! `POST /v0/publish`. Retry windows are clamped to 1ms so a "5xx then 2xx"
//! scenario completes in a few milliseconds rather than waiting on the
//! default 10s base delay.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anodizer_core::config::{
    Config, HumanDuration, McpAuthMethod, McpConfig, McpPackage, McpRegistryType, McpTransport,
    McpTransportType, RetryConfig, StringOrBool,
};
use anodizer_core::context::{Context, ContextOptions};

use super::{publish_with_registry, reset_experimental_warned_for_test, warn_experimental_once};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Spawn a one-shot HTTP responder on `127.0.0.1:0` that returns each
/// configured raw HTTP/1.1 response to successive connections, then exits.
/// Returns the bound address and a counter so a test can assert the exact
/// number of attempts the publisher made.
///
/// Mirrors the pattern in `dockerhub.rs::spawn_oneshot_http_responder` —
/// keeping the two harnesses identical means we can lift this to a shared
/// helper once a third HTTP publisher reuses it.
fn spawn_oneshot_http_responder(responses: Vec<&'static str>) -> (SocketAddr, Arc<AtomicU32>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let counter = Arc::new(AtomicU32::new(0));
    let counter_inner = counter.clone();
    std::thread::spawn(move || {
        for (i, resp) in responses.iter().enumerate() {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            counter_inner.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 8192];
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if i == responses.len() - 1 {
                break;
            }
        }
    });
    (addr, counter)
}

/// Build a minimal context with a sufficiently-configured `mcp:` block to
/// reach the publish loop. `name`, `auth.token`, `packages[0]` all populated.
/// The version is set to "1.0.0" so the published payload has a non-empty
/// `version` field (matching GR's behaviour — `mcp.go::Publish` reads
/// `ctx.Version` unconditionally).
fn mcp_ctx(mcp_overrides: impl FnOnce(&mut McpConfig)) -> Context {
    let mut config = Config::default();
    config.project_name = "anodizer".to_string();
    // Use a tight retry policy so a retry test completes in ms — the default
    // 10-attempt 10s-base policy would block the test runner for minutes.
    config.retry = Some(RetryConfig {
        attempts: 3,
        delay: HumanDuration(Duration::from_millis(1)),
        max_delay: HumanDuration(Duration::from_millis(5)),
    });

    config.mcp = McpConfig {
        name: Some("io.github.test/server".to_string()),
        description: Some("Test server".to_string()),
        title: None,
        homepage: None,
        packages: vec![McpPackage {
            registry_type: McpRegistryType::Oci,
            identifier: "ghcr.io/test/server:v1".to_string(),
            transport: McpTransport {
                kind: McpTransportType::Stdio,
            },
        }],
        transports: vec![],
        skip: None,
        repository: Default::default(),
        auth: anodizer_core::config::McpAuth {
            method: McpAuthMethod::None,
            // Non-empty token short-circuits NoneAuthProvider — no
            // `/v0/auth/none` round-trip in tests, just `/v0/publish`.
            token: "preissued-jwt".to_string(),
        },
        registry: None,
    };
    mcp_overrides(&mut config.mcp);

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx
}

// ---------------------------------------------------------------------------
// Skip-gate parity
// ---------------------------------------------------------------------------

#[test]
fn skip_when_no_name() {
    // GR mcp.go::Skip parity: an empty `name` skips the entire publisher
    // BEFORE any token exchange or network call. The responder is bound but
    // intentionally never accepts a connection — the test would hang on
    // `accept()` if the publisher tried to POST. The counter must read 0.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);
    let registry = format!("http://{addr}");

    let ctx = mcp_ctx(|mcp| {
        mcp.name = None;
    });
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&ctx, &log, &registry);
    assert!(result.is_ok(), "skip path must not error: {:?}", result);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no HTTP calls must be made when mcp.name is empty"
    );
}

#[test]
fn skip_when_skip_evaluates_true() {
    // skip: "{{ true }}" → publisher returns Ok(()) and emits no HTTP
    // calls. Mirrors the standard `--skip=mcp` semantics enforced by every
    // top-level publisher.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);
    let registry = format!("http://{addr}");

    let ctx = mcp_ctx(|mcp| {
        mcp.skip = Some(StringOrBool::String("{{ true }}".to_string()));
    });
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&ctx, &log, &registry);
    assert!(result.is_ok(), "skip=true must skip cleanly: {:?}", result);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no HTTP calls when skip evaluates true"
    );
}

// ---------------------------------------------------------------------------
// Publish loop — retries
// ---------------------------------------------------------------------------

#[test]
fn publish_retries_on_500_then_succeeds() {
    // wiremock-equivalent: 500 then 201. With a 3-attempt 1ms-base policy
    // this completes in low single-digit ms. Mirrors the GR
    // `TestPublishRetryable` behaviour — `retry_http_blocking` classifies
    // 5xx as Continue and 2xx as success.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
    ]);
    let registry = format!("http://{addr}");

    let ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&ctx, &log, &registry);
    assert!(result.is_ok(), "5xx then 2xx must succeed: {:?}", result);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one 500 retry then 201 success"
    );
}

#[test]
fn publish_unrecoverable_on_400() {
    // 4xx is Break (fast-fail) — the retry helper classifies it as
    // unrecoverable so a bad payload surfaces immediately instead of
    // burning the full retry budget. With responses limited to 1, a
    // second `accept()` would block; the test passing the assert proves
    // we didn't retry.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 13\r\n\r\nbad payload\r\n",
    ]);
    let registry = format!("http://{addr}");

    let ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&ctx, &log, &registry);
    let err = result.expect_err("400 must surface as an error");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("400") || chain.contains("bad payload"),
        "error chain must surface the HTTP status / body: {chain}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "4xx must NOT retry — exactly one call"
    );
}

// ---------------------------------------------------------------------------
// Experimental-warning one-shot semantics
// ---------------------------------------------------------------------------

#[test]
fn experimental_warning_emitted_once_per_process() {
    // The atomic flag is a process-wide one-shot. Reset it (test-only
    // helper) then call `warn_experimental_once` twice; only the first
    // call should be observable. We can't intercept the StageLogger's
    // stderr easily, so we assert the atomic itself flipped — first call
    // returns the prior `false` and sets it to `true`; second call
    // observes `true` and short-circuits.
    use std::sync::atomic::Ordering;

    reset_experimental_warned_for_test();
    let ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");

    // First invocation flips the flag from false → true (and emits).
    warn_experimental_once(&log);
    let after_first = super::EXPERIMENTAL_WARNED.load(Ordering::SeqCst);
    assert!(after_first, "first call must set the flag");

    // Subsequent invocations are silent — the flag stays true and no
    // emit happens. We verify via the atomic; once the flag is true, the
    // function's swap-then-check sees `prior == true` and returns.
    warn_experimental_once(&log);
    warn_experimental_once(&log);
    let after_many = super::EXPERIMENTAL_WARNED.load(Ordering::SeqCst);
    assert!(after_many, "flag stays true across repeated calls");
}
