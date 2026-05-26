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

#![allow(clippy::field_reassign_with_default)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use anodizer_core::config::{
    Config, HumanDuration, McpAuthMethod, McpConfig, McpPackage, McpRegistryType, McpTransport,
    McpTransportType, ReleaseConfig, RetryConfig, ScmRepoConfig, StringOrBool,
};
use anodizer_core::context::{Context, ContextOptions};

use super::{
    DEFAULT_REGISTRY_URL, fill_from_project_metadata, infer_repository_from_release,
    publish_with_registry, reset_experimental_warned_for_test, resolve_registry_url,
    warn_experimental_once, warn_once_lock,
};
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

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
        packages: vec![McpPackage {
            registry_type: McpRegistryType::Oci,
            identifier: "ghcr.io/test/server:v1".to_string(),
            transport: McpTransport {
                kind: McpTransportType::Stdio,
            },
        }],
        auth: anodizer_core::config::McpAuth {
            method: McpAuthMethod::None,
            token: "preissued-jwt".to_string(),
        },
        ..Default::default()
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
    let _g = warn_once_lock();
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
    let _g = warn_once_lock();
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
    let _g = warn_once_lock();
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
    let _g = warn_once_lock();
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
    // The atomic flag is a process-wide one-shot. `warn_experimental_once`
    // returns `true` exactly when this call flipped the flag (and emitted
    // the warning). Race-safe: we depend on the function's per-call return
    // value, not on inspecting the static atomic — which other parallel
    // tests (publish_retries_*, dry_run_*) could already have flipped via
    // their internal call. The reset_experimental_warned_for_test() helper
    // forces a known starting state but offers no protection against a
    // concurrent test flipping the flag back between our calls, so we
    // assert the boolean returns instead.
    let _g = warn_once_lock();
    reset_experimental_warned_for_test();
    let ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");

    // Exactly one call should observe `true`; the rest observe `false`.
    let emits: Vec<bool> = (0..3).map(|_| warn_experimental_once(&log)).collect();
    let true_count = emits.iter().filter(|&&b| b).count();
    assert_eq!(
        true_count, 1,
        "expected exactly one true (first-emitter) across three calls; got {emits:?}"
    );
}

// ---------------------------------------------------------------------------
// Dry-run short-circuit
// ---------------------------------------------------------------------------

#[test]
fn dry_run_short_circuits_before_network() {
    // Per mcp/mod.rs:106 — when ctx.is_dry_run() is true the publisher
    // logs the intended POST and returns Ok(()) without contacting the
    // registry. We bind a listener that intentionally never serves any
    // response; if the publisher tried to POST, accept() would happen
    // and the counter would tick.
    let _g = warn_once_lock();
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);
    let registry = format!("http://{addr}");

    let mut config = Config::default();
    config.project_name = "anodizer".to_string();
    config.retry = Some(RetryConfig {
        attempts: 3,
        delay: HumanDuration(Duration::from_millis(1)),
        max_delay: HumanDuration(Duration::from_millis(5)),
    });
    config.mcp = McpConfig {
        name: Some("io.github.test/server".to_string()),
        description: Some("Test server".to_string()),
        packages: vec![McpPackage {
            registry_type: McpRegistryType::Oci,
            identifier: "ghcr.io/test/server:v1".to_string(),
            transport: McpTransport {
                kind: McpTransportType::Stdio,
            },
        }],
        auth: anodizer_core::config::McpAuth {
            method: McpAuthMethod::None,
            token: "preissued-jwt".to_string(),
        },
        ..Default::default()
    };

    let opts = ContextOptions {
        dry_run: true,
        ..ContextOptions::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Version", "1.0.0");

    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&ctx, &log, &registry);
    assert!(result.is_ok(), "dry-run must return Ok(()): {:?}", result);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "dry-run must NOT contact the registry (0 accepts); got {:?}",
        calls.load(Ordering::SeqCst)
    );
}

// ---------------------------------------------------------------------------
// Repository inference from release context
// ---------------------------------------------------------------------------

/// Build a `Config` carrying only a `release:` block for the given SCM
/// host. Centralizes the struct-update boilerplate the inference tests
/// share (avoids clippy `field_reassign_with_default`).
fn cfg_with_release(host: &str, owner: &str, name: &str) -> Config {
    let repo = Some(ScmRepoConfig {
        owner: owner.to_string(),
        name: name.to_string(),
    });
    let release = match host {
        "github" => ReleaseConfig {
            github: repo,
            ..ReleaseConfig::default()
        },
        "gitlab" => ReleaseConfig {
            gitlab: repo,
            ..ReleaseConfig::default()
        },
        "gitea" => ReleaseConfig {
            gitea: repo,
            ..ReleaseConfig::default()
        },
        other => panic!("cfg_with_release: unknown host {other:?}"),
    };
    Config {
        release: Some(release),
        ..Config::default()
    }
}

#[test]
fn infer_repository_github_from_release_config() {
    // When release.github.{owner,name} is set and mcp.repository.url is
    // empty, inference must populate repository.url + repository.source.
    let ctx = Context::new(
        cfg_with_release("github", "myorg", "myapp"),
        ContextOptions::default(),
    );

    let mut mcp = McpConfig::default();
    infer_repository_from_release(&ctx, &mut mcp);
    assert_eq!(mcp.repository.url, "https://github.com/myorg/myapp");
    assert_eq!(mcp.repository.source, "github");
}

#[test]
fn infer_repository_gitlab_from_release_config() {
    let ctx = Context::new(
        cfg_with_release("gitlab", "myorg", "myapp"),
        ContextOptions::default(),
    );

    let mut mcp = McpConfig::default();
    infer_repository_from_release(&ctx, &mut mcp);
    assert_eq!(mcp.repository.url, "https://gitlab.com/myorg/myapp");
    assert_eq!(mcp.repository.source, "gitlab");
}

#[test]
fn infer_repository_gitea_from_release_config() {
    let ctx = Context::new(
        cfg_with_release("gitea", "myorg", "myapp"),
        ContextOptions::default(),
    );

    let mut mcp = McpConfig::default();
    infer_repository_from_release(&ctx, &mut mcp);
    assert_eq!(mcp.repository.url, "https://gitea.com/myorg/myapp");
    assert_eq!(mcp.repository.source, "gitea");
}

#[test]
fn inference_does_not_override_explicit_repository() {
    // If the user set mcp.repository.url explicitly, inference must
    // leave both url and source untouched even when release.github is
    // also set.
    let ctx = Context::new(
        cfg_with_release("github", "myorg", "myapp"),
        ContextOptions::default(),
    );

    let mut mcp = McpConfig::default();
    mcp.repository.url = "https://custom.example.com/myorg/myapp".to_string();
    mcp.repository.source = "custom".to_string();
    infer_repository_from_release(&ctx, &mut mcp);
    assert_eq!(
        mcp.repository.url, "https://custom.example.com/myorg/myapp",
        "explicit URL must win"
    );
    assert_eq!(mcp.repository.source, "custom", "explicit source must win");
}

#[test]
fn inference_no_ops_when_owner_or_name_empty() {
    // Defensive: an empty owner OR name in release.github must not
    // produce a half-baked URL like https://github.com//repo. The
    // function must return without touching mcp.repository.
    for (owner, name) in [("", "repo"), ("owner", ""), ("", "")] {
        let ctx = Context::new(
            cfg_with_release("github", owner, name),
            ContextOptions::default(),
        );

        let mut mcp = McpConfig::default();
        infer_repository_from_release(&ctx, &mut mcp);
        assert!(
            mcp.repository.url.is_empty(),
            "owner={owner:?} name={name:?}: url must stay empty, got {:?}",
            mcp.repository.url
        );
        assert!(
            mcp.repository.source.is_empty(),
            "owner={owner:?} name={name:?}: source must stay empty, got {:?}",
            mcp.repository.source
        );
    }
}

// ---------------------------------------------------------------------------
// Project-metadata fallback
// ---------------------------------------------------------------------------

/// When `mcp.description` is unset, the publisher must fall back to the
/// top-level `metadata.description`. Same fallback shape used by every
/// other publisher (homebrew cask, dockerhub, snapcraft).
#[test]
fn mcp_inherits_meta_description_when_unset() {
    use anodizer_core::config::MetadataConfig;

    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("from project metadata".to_string()),
        homepage: Some("https://example.com/project".to_string()),
        ..Default::default()
    });

    // Per-MCP description / homepage left None — fallback must kick in.
    let mut mcp = McpConfig::default();
    let ctx = Context::new(config, ContextOptions::default());
    fill_from_project_metadata(&ctx, &mut mcp);
    assert_eq!(
        mcp.description.as_deref(),
        Some("from project metadata"),
        "missing mcp.description must inherit metadata.description"
    );
    assert_eq!(
        mcp.homepage.as_deref(),
        Some("https://example.com/project"),
        "missing mcp.homepage must inherit metadata.homepage"
    );
}

/// Empty per-MCP description (literally `Some("")`) falls back too —
/// the helper treats empty-string the same as `None`.
#[test]
fn mcp_empty_description_falls_back_to_meta() {
    use anodizer_core::config::MetadataConfig;

    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("project description".to_string()),
        ..Default::default()
    });

    let mut mcp = McpConfig::default();
    mcp.description = Some(String::new());
    let ctx = Context::new(config, ContextOptions::default());
    fill_from_project_metadata(&ctx, &mut mcp);
    assert_eq!(mcp.description.as_deref(), Some("project description"));
}

/// Explicit per-MCP description wins over the metadata fallback.
#[test]
fn mcp_explicit_description_wins_over_meta() {
    use anodizer_core::config::MetadataConfig;

    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("metadata fallback".to_string()),
        ..Default::default()
    });

    let mut mcp = McpConfig::default();
    mcp.description = Some("explicit mcp value".to_string());
    let ctx = Context::new(config, ContextOptions::default());
    fill_from_project_metadata(&ctx, &mut mcp);
    assert_eq!(mcp.description.as_deref(), Some("explicit mcp value"));
}

// ---------------------------------------------------------------------------
// Registry URL fallback
// ---------------------------------------------------------------------------

#[test]
fn resolve_registry_url_fallback_matrix() {
    // The fallback chain is load-bearing: empty/whitespace/None all must
    // collapse to DEFAULT_REGISTRY_URL so a user who left `mcp.registry`
    // commented out (or templated to an empty string under a conditional)
    // still gets a working publish. An explicit override wins verbatim.
    let cases: &[(Option<&str>, &str, &str)] = &[
        (None, DEFAULT_REGISTRY_URL, "None → default"),
        (Some(""), DEFAULT_REGISTRY_URL, "empty → default"),
        (Some("   "), DEFAULT_REGISTRY_URL, "whitespace → default"),
        (
            Some("https://staging.example.com"),
            "https://staging.example.com",
            "explicit override wins",
        ),
    ];
    for (input, expected, label) in cases {
        let mcp = McpConfig {
            registry: input.map(|s| s.to_string()),
            ..Default::default()
        };
        let got = resolve_registry_url(&mcp);
        assert_eq!(got, *expected, "case {label}: input={input:?}");
    }
}
