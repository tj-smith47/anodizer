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
    DEFAULT_REGISTRY_URL, MAX_RESPONSE_SNIPPET_BYTES, apply_inferred_repository,
    fill_from_project_metadata, image_repo, infer_repository_from_release,
    is_duplicate_version_rejection, mcp_image_owned_by_selected, oci_rejection_hint,
    publish_with_registry, reset_experimental_warned_for_test, resolve_registry_url,
    truncate_response_snippet, warn_experimental_once, warn_once_lock,
};
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Build a minimal context with a sufficiently-configured `mcp:` block to
/// reach the publish loop. `name`, `auth.token`, `packages[0]` all populated.
/// The version is set to "1.0.0" so the published payload has a non-empty
/// `version` field (the publish path reads
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
        max_elapsed: None,
    });

    config.mcp = McpConfig {
        name: Some("io.github.test/server".to_string()),
        description: Some("Test server".to_string()),
        packages: vec![McpPackage {
            registry_type: McpRegistryType::Oci,
            identifier: "ghcr.io/test/server:v1".to_string(),
            transport: McpTransport {
                kind: McpTransportType::Stdio,
                ..McpTransport::default()
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
    // An empty `name` skips the entire publisher
    // BEFORE any token exchange or network call. The responder is bound but
    // intentionally never accepts a connection — the test would hang on
    // `accept()` if the publisher tried to POST. The counter must read 0.
    let _g = warn_once_lock();
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);
    let registry = format!("http://{addr}");

    let mut ctx = mcp_ctx(|mcp| {
        mcp.name = None;
    });
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
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

    let mut ctx = mcp_ctx(|mcp| {
        mcp.skip = Some(StringOrBool::String("{{ true }}".to_string()));
    });
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
    assert!(result.is_ok(), "skip=true must skip cleanly: {:?}", result);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no HTTP calls when skip evaluates true"
    );
}

#[test]
fn skip_true_records_skipped_outcome() {
    // A `skip: true` mcp publisher must record a `Skipped` outcome before
    // returning `Ok(None)`. Without it the dispatch layer defaults the
    // outcome to `Succeeded` and the summary reports a skipped server as
    // published.
    let _g = warn_once_lock();
    let (addr, _calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);
    let registry = format!("http://{addr}");

    let mut ctx = mcp_ctx(|mcp| {
        mcp.skip = Some(StringOrBool::String("{{ true }}".to_string()));
    });
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
    assert!(result.is_ok(), "skip=true must skip cleanly: {:?}", result);
    assert!(
        matches!(
            ctx.take_pending_outcome(),
            Some(anodizer_core::PublisherOutcome::Skipped(_))
        ),
        "skip=true must record a Skipped outcome, not default to Succeeded"
    );
}

#[test]
fn if_falsy_records_skipped_outcome() {
    // An `if:` condition that evaluates falsy must record a `Skipped`
    // outcome before returning `Ok(None)` — same reasoning as the
    // `skip: true` path.
    let _g = warn_once_lock();
    let (addr, _calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);
    let registry = format!("http://{addr}");

    let mut ctx = mcp_ctx(|mcp| {
        mcp.if_condition = Some("{{ false }}".to_string());
    });
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
    assert!(result.is_ok(), "if-falsy must skip cleanly: {:?}", result);
    assert!(
        matches!(
            ctx.take_pending_outcome(),
            Some(anodizer_core::PublisherOutcome::Skipped(_))
        ),
        "if-falsy must record a Skipped outcome, not default to Succeeded"
    );
}

#[test]
fn metadata_fallback_is_rendered_in_published_body() {
    // A templated project-metadata fallback (e.g. `metadata.homepage`
    // carrying a `{{ .Version }}` token) must be rendered before the POST,
    // not shipped raw. The metadata fill therefore runs BEFORE
    // `render_strings`, mirroring scoop's ordering.
    let _g = warn_once_lock();
    let (addr, captured) =
        anodizer_core::test_helpers::responder::spawn_request_capturing_responder(
            "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        );
    let registry = format!("http://{addr}");

    let mut ctx = mcp_ctx(|mcp| {
        // Leave homepage unset so the project-metadata fallback fires.
        mcp.homepage = None;
    });
    ctx.config.metadata = Some(anodizer_core::config::MetadataConfig {
        homepage: Some("https://example.com/{{ .Version }}".to_string()),
        ..Default::default()
    });
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
    assert!(result.is_ok(), "publish must succeed: {:?}", result);

    let body = captured.lock().unwrap().clone();
    assert!(
        body.contains("https://example.com/1.0.0"),
        "metadata homepage fallback must be rendered (1.0.0), got body:\n{}",
        body
    );
    assert!(
        !body.contains("{{ .Version }}"),
        "raw template token must not ship in the published body:\n{}",
        body
    );
}

// ---------------------------------------------------------------------------
// Publish loop — retries
// ---------------------------------------------------------------------------

#[test]
fn publish_retries_on_500_then_succeeds() {
    // wiremock-equivalent: 500 then 201. With a 3-attempt 1ms-base policy
    // this completes in low single-digit ms. The
    // `TestPublishRetryable` behaviour — `retry_http_blocking` classifies
    // 5xx as Continue and 2xx as success.
    let _g = warn_once_lock();
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
    ]);
    let registry = format!("http://{addr}");

    let mut ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
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

    let mut ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
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

#[test]
fn duplicate_version_400_is_idempotent_skip() {
    // Re-running a release at an already-published version: the registry
    // rejects the re-POST with 400 `cannot publish duplicate version`. That
    // is the desired end-state on a re-run, so the publisher must record a
    // SKIP (success), return Ok(None) (NO rollback target), and NOT surface
    // an error.
    let _g = warn_once_lock();
    // Body is 76 bytes; Content-Length pins it so the responder frames it.
    const RESPONSE: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 76\r\n\r\n\
        {\"errors\":[{\"message\":\"invalid version: cannot publish duplicate version\"}]}";
    let (addr, calls) = spawn_oneshot_http_responder(vec![RESPONSE]);
    let registry = format!("http://{addr}");

    let mut ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");
    let result = publish_with_registry(&mut ctx, &log, &registry);
    let target = result.expect("duplicate version must be a clean skip, not an error");
    assert!(
        target.is_none(),
        "already-published must record NO rollback target"
    );
    assert!(
        matches!(
            ctx.take_pending_outcome(),
            Some(anodizer_core::PublisherOutcome::Skipped(
                anodizer_core::SkipReason::AlreadyPublished
            ))
        ),
        "duplicate version must record Skipped(AlreadyPublished)"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "duplicate-version 400 must NOT retry"
    );
}

#[test]
fn is_duplicate_version_rejection_matches_signal_not_other_400s() {
    // Parsed envelope match.
    assert!(is_duplicate_version_rejection(
        400,
        r#"{"errors":[{"message":"invalid version: cannot publish duplicate version"}]}"#
    ));
    // Case-insensitive / reworded wrapper, whole-body fallback.
    assert!(is_duplicate_version_rejection(
        400,
        "Duplicate Version already exists"
    ));
    // A different 400 (schema rejection) must NOT be classified as a skip.
    assert!(!is_duplicate_version_rejection(
        400,
        r#"{"errors":[{"message":"body.packages[0].version: minLength"}]}"#
    ));
    // The same words on a non-400 status are a different failure mode and
    // must still surface as an error.
    assert!(!is_duplicate_version_rejection(
        409,
        "cannot publish duplicate version"
    ));
    assert!(!is_duplicate_version_rejection(
        500,
        "cannot publish duplicate version"
    ));
}

#[test]
fn oci_annotation_rejection_appends_actionable_hint() {
    // The registry's OCI validator fails closed when the published image
    // lacks the `io.modelcontextprotocol.server.name` config label, returning
    // a body that names the label. The raw registry text only mentions a
    // Dockerfile LABEL; anodizer must add the `dockers_v2.labels` path so users
    // who build images via the `dockers_v2:` block know where to set it.
    let _g = warn_once_lock();
    // Content-Length is the 161-byte body that follows the blank line — the
    // registry's verbatim "missing required annotation" text naming the label.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 161\r\n\r\n\
         OCI image 'ghcr.io/test/server:v1' is missing required annotation. \
         Add this to your Dockerfile: LABEL io.modelcontextprotocol.server.name=\"io.github.test/server\"",
    ]);
    let registry = format!("http://{addr}");

    let mut ctx = mcp_ctx(|_| {});
    let log = ctx.logger("mcp-test");
    let err = publish_with_registry(&mut ctx, &log, &registry)
        .expect_err("422 annotation rejection must surface as an error");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("dockers_v2.labels"),
        "error must point at dockers_v2.labels remediation: {chain}"
    );
    assert!(
        chain.contains("io.github.test/server"),
        "hint must quote the server name the label must equal: {chain}"
    );
    assert!(
        chain.contains("NOT `annotations`"),
        "hint must warn that annotations are ignored by the validator: {chain}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "4xx must NOT retry — exactly one call"
    );
}

#[test]
fn oci_rejection_hint_is_empty_for_unrelated_bodies() {
    // A plain bad-request body that does not name the ownership label must
    // produce no hint — the hint is reserved for the annotation-missing case
    // so it never muddies unrelated 4xx diagnostics.
    assert_eq!(
        oci_rejection_hint("bad payload", "io.github.test/server"),
        ""
    );
    let hint = oci_rejection_hint(
        "OCI image is missing required annotation: io.modelcontextprotocol.server.name",
        "io.github.test/server",
    );
    assert!(hint.contains("io.github.test/server"), "{hint}");
    assert!(hint.contains("dockers_v2.labels"), "{hint}");
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
        max_elapsed: None,
    });
    config.mcp = McpConfig {
        name: Some("io.github.test/server".to_string()),
        description: Some("Test server".to_string()),
        packages: vec![McpPackage {
            registry_type: McpRegistryType::Oci,
            identifier: "ghcr.io/test/server:v1".to_string(),
            transport: McpTransport {
                kind: McpTransportType::Stdio,
                ..McpTransport::default()
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
    let result = publish_with_registry(&mut ctx, &log, &registry);
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
        token: None,
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
#[serial_test::serial]
fn inference_no_ops_when_owner_or_name_empty() {
    // Defensive: an empty owner OR name in release.github must not
    // produce a half-baked URL like https://github.com//repo. Run inside a
    // remote-less git repo so the git-remote fallback also yields nothing,
    // isolating the release-block branch under test.
    let repo = temp_git_repo_no_remote();
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(repo.path()).expect("cwd guard");

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
// Repository derivation from the git remote (fallback when no release block)
// ---------------------------------------------------------------------------

/// Init a throwaway git repo with the given `origin` remote and return the
/// tempdir handle (kept alive by the caller).
fn temp_git_repo_with_remote(remote_url: &str) -> tempfile::TempDir {
    use std::process::Command;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["init", "-q"]).current_dir(path);
                cmd
            },
            "git",
        )
        .status
        .success()
    );
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["remote", "add", "origin", remote_url])
                    .current_dir(path);
                cmd
            },
            "git",
        )
        .status
        .success()
    );
    dir
}

/// Init a throwaway git repo with NO `origin` remote — the git-remote
/// fallback resolves to `None` from inside it.
fn temp_git_repo_no_remote() -> tempfile::TempDir {
    use std::process::Command;
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["init", "-q"]).current_dir(dir.path());
                cmd
            },
            "git",
        )
        .status
        .success()
    );
    dir
}

#[test]
#[serial_test::serial]
fn infer_repository_derives_from_github_remote_when_no_release_block() {
    // No release block at all: the git-remote fallback must derive
    // url + source="github" from a GitHub `origin`.
    let repo = temp_git_repo_with_remote("git@github.com:acme/widget.git");
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(repo.path()).expect("cwd guard");

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let mut mcp = McpConfig::default();
    infer_repository_from_release(&ctx, &mut mcp);
    assert_eq!(mcp.repository.url, "https://github.com/acme/widget");
    assert_eq!(mcp.repository.source, "github");
}

#[test]
#[serial_test::serial]
fn infer_repository_not_forced_for_non_github_remote() {
    // A self-hosted / non-GitHub remote must NOT be force-derived — the
    // repository object stays user-supplied (here: empty).
    let repo = temp_git_repo_with_remote("git@gitlab.example.com:acme/widget.git");
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(repo.path()).expect("cwd guard");

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let mut mcp = McpConfig::default();
    infer_repository_from_release(&ctx, &mut mcp);
    assert!(
        mcp.repository.url.is_empty(),
        "non-github remote must not force a url, got {:?}",
        mcp.repository.url
    );
    assert!(mcp.repository.source.is_empty());
}

#[test]
#[serial_test::serial]
fn infer_repository_user_set_wins_over_github_remote() {
    // An explicit repository.url must survive even when a GitHub remote
    // could otherwise be derived.
    let repo = temp_git_repo_with_remote("git@github.com:acme/widget.git");
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(repo.path()).expect("cwd guard");

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let mut mcp = McpConfig::default();
    mcp.repository.url = "https://example.com/me/thing".to_string();
    mcp.repository.source = "custom".to_string();
    infer_repository_from_release(&ctx, &mut mcp);
    assert_eq!(mcp.repository.url, "https://example.com/me/thing");
    assert_eq!(mcp.repository.source, "custom");
}

#[test]
fn apply_inferred_repository_preserves_user_source() {
    // The pure writer must keep a user-set source while filling the url.
    let mut mcp = McpConfig::default();
    mcp.repository.source = "ghe".to_string();
    apply_inferred_repository(
        &mut mcp,
        Some(("github", "acme".to_string(), "widget".to_string())),
    );
    assert_eq!(mcp.repository.url, "https://github.com/acme/widget");
    assert_eq!(mcp.repository.source, "ghe", "user-set source must win");
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

// ---------------------------------------------------------------------------
// HTTP error-snippet truncation
// ---------------------------------------------------------------------------

#[test]
fn truncate_response_snippet_short_body_returned_verbatim() {
    // Bodies at or below the byte cap pass through unchanged with an
    // empty suffix — no allocation surprise, no truncation marker.
    let body = "ok";
    let (snippet, suffix) = truncate_response_snippet(body);
    assert_eq!(snippet, "ok");
    assert_eq!(suffix, "");
}

#[test]
fn truncate_response_snippet_at_cap_returned_verbatim() {
    // Exactly-cap bodies are NOT truncated. Guards the `<=` boundary so
    // a hypothetical future off-by-one doesn't silently start tagging
    // every cap-sized response as truncated.
    let body = "a".repeat(MAX_RESPONSE_SNIPPET_BYTES);
    let (snippet, suffix) = truncate_response_snippet(&body);
    assert_eq!(snippet.len(), MAX_RESPONSE_SNIPPET_BYTES);
    assert_eq!(suffix, "");
}

#[test]
fn truncate_response_snippet_oversized_ascii_marks_truncated() {
    // ASCII path: every byte is a char boundary so the cut lands on
    // exactly `MAX_RESPONSE_SNIPPET_BYTES` and the suffix announces
    // the trim.
    let body = "x".repeat(MAX_RESPONSE_SNIPPET_BYTES * 2);
    let (snippet, suffix) = truncate_response_snippet(&body);
    assert_eq!(snippet.len(), MAX_RESPONSE_SNIPPET_BYTES);
    assert_eq!(suffix, "...[truncated]");
}

#[test]
fn truncate_response_snippet_walks_back_to_utf8_char_boundary() {
    // A 4-byte UTF-8 char straddling the cap forces the cut to walk
    // backward to a boundary. The snippet must be valid UTF-8 (the
    // `format!` formatter would panic on a slice through a multi-byte
    // char), strictly shorter than the cap, and end on a clean
    // codepoint — not mid-sequence.
    let mut body = "a".repeat(MAX_RESPONSE_SNIPPET_BYTES - 2);
    // U+1F600 GRINNING FACE is 4 bytes in UTF-8. With 510 leading 'a's
    // the char straddles bytes 510..514, so a 512-byte cut lands inside
    // it and the loop must walk back to byte 510.
    body.push('\u{1F600}');
    body.push_str(&"b".repeat(20));
    let (snippet, suffix) = truncate_response_snippet(&body);

    assert_eq!(suffix, "...[truncated]");
    assert!(
        snippet.len() < MAX_RESPONSE_SNIPPET_BYTES,
        "boundary walk must yield strictly fewer bytes than the cap (got {})",
        snippet.len()
    );
    // The smiley straddles the cap, so it must NOT appear in the snippet
    // — otherwise the cut landed past the boundary, not before it.
    assert!(
        !snippet.contains('\u{1F600}'),
        "the multi-byte char straddling the cap must be dropped wholesale, not split"
    );
    // Round-trip back into a format! to prove the result is valid UTF-8
    // (a mid-codepoint slice would panic here, even though indexing in
    // the helper itself uses a byte slice). The assertion is the
    // absence of a panic.
    let _ = format!("snippet={snippet}");
}

// ---------------------------------------------------------------------------
// Owning-crate gate (per-crate iteration)
// ---------------------------------------------------------------------------

#[test]
fn image_repo_strips_tag_keeps_registry_port_and_digest() {
    assert_eq!(image_repo("ghcr.io/owner/app:v1.2.3"), "ghcr.io/owner/app");
    assert_eq!(image_repo("ghcr.io/owner/app"), "ghcr.io/owner/app");
    // A registry host port must not be mistaken for a tag.
    assert_eq!(
        image_repo("registry:5000/owner/app"),
        "registry:5000/owner/app"
    );
    assert_eq!(
        image_repo("registry:5000/owner/app:latest"),
        "registry:5000/owner/app"
    );
    // An un-rendered template tag is stripped just like a literal tag.
    assert_eq!(
        image_repo("ghcr.io/owner/app:{{ .Version }}"),
        "ghcr.io/owner/app"
    );
    // Digest references trim back to the repo.
    assert_eq!(
        image_repo("ghcr.io/owner/app@sha256:deadbeef"),
        "ghcr.io/owner/app"
    );
}

/// Build a context whose top-level crates each carry a `docker_v2` image,
/// with the given `selected_crates` scope and an mcp OCI package pointing
/// at `mcp_identifier`.
fn owning_ctx(selected: Vec<&str>, mcp_identifier: &str) -> Context {
    use anodizer_core::config::{CrateConfig, DockerV2Config};

    let docker_for = |image: &str| {
        Some(vec![DockerV2Config {
            images: vec![image.to_string()],
            ..Default::default()
        }])
    };
    let mut config = Config::default();
    config.crates = vec![
        CrateConfig {
            name: "cfgd-core".to_string(),
            dockers_v2: None,
            ..Default::default()
        },
        CrateConfig {
            name: "cfgd".to_string(),
            dockers_v2: docker_for("ghcr.io/tj-smith47/cfgd"),
            ..Default::default()
        },
    ];
    config.mcp = McpConfig {
        name: Some("io.github.test/server".to_string()),
        packages: vec![McpPackage {
            registry_type: McpRegistryType::Oci,
            identifier: mcp_identifier.to_string(),
            transport: McpTransport {
                kind: McpTransportType::Stdio,
                ..McpTransport::default()
            },
        }],
        ..Default::default()
    };
    let opts = ContextOptions {
        selected_crates: selected.into_iter().map(String::from).collect(),
        ..ContextOptions::default()
    };
    Context::new(config, opts)
}

#[test]
fn mcp_runs_for_owning_crate_pass() {
    // The `cfgd` crate's docker_v2 image base matches the mcp identifier's
    // repo (tag-stripped), so mcp must run on its pass.
    let ctx = owning_ctx(vec!["cfgd"], "ghcr.io/tj-smith47/cfgd:{{ .Version }}");
    assert!(mcp_image_owned_by_selected(&ctx));
}

#[test]
fn mcp_skips_for_non_owning_crate_pass() {
    // `cfgd-core` owns no docker_v2 image matching the mcp identifier, so
    // mcp must be skipped during its pass rather than firing spuriously.
    let ctx = owning_ctx(vec!["cfgd-core"], "ghcr.io/tj-smith47/cfgd:{{ .Version }}");
    assert!(!mcp_image_owned_by_selected(&ctx));
}

#[test]
fn mcp_non_owning_pass_records_skipped_not_applicable() {
    // The non-owning pass must surface `Skipped(NotApplicable)` so the
    // publisher summary reads "Skipped" rather than a green "Succeeded" —
    // mcp did not publish for this crate.
    let mut ctx = owning_ctx(vec!["cfgd-core"], "ghcr.io/tj-smith47/cfgd:{{ .Version }}");
    let log = ctx.logger("mcp-test");
    let target = super::publish_to_mcp(&mut ctx, &log).expect("ok");
    assert!(target.is_none(), "non-owning pass produces no target");
    assert!(matches!(
        ctx.take_pending_outcome(),
        Some(anodizer_core::PublisherOutcome::Skipped(
            anodizer_core::SkipReason::NotApplicable
        ))
    ));
}

#[test]
fn mcp_runs_when_no_crate_selection() {
    // Empty `selected_crates` (non-workspace / run-once) keeps the
    // historical run-once behavior regardless of image ownership.
    let ctx = owning_ctx(vec![], "ghcr.io/unrelated/image:v1");
    assert!(mcp_image_owned_by_selected(&ctx));
}

#[test]
fn mcp_runs_when_manifest_has_no_oci_package() {
    // With a non-OCI package the ownership concept does not apply; every
    // selected pass is allowed through so the skip-gate decides.
    let mut ctx = owning_ctx(vec!["cfgd-core"], "unused");
    ctx.config.mcp.packages = vec![McpPackage {
        registry_type: McpRegistryType::Npm,
        identifier: "some-npm-pkg".to_string(),
        transport: McpTransport {
            kind: McpTransportType::Stdio,
            ..McpTransport::default()
        },
    }];
    assert!(mcp_image_owned_by_selected(&ctx));
}

#[test]
fn render_strings_renders_header_values_but_not_names() {
    // Transport header NAMES are literal protocol identifiers — published
    // exactly as written, never template-rendered — while header VALUES
    // are rendered. Pins the documented contract in both directions.
    use anodizer_core::config::McpHeader;

    let mut ctx = mcp_ctx(|mcp| {
        mcp.packages[0].transport = McpTransport {
            kind: McpTransportType::StreamableHttp,
            url: "https://example.com/v1".to_string(),
            headers: vec![McpHeader {
                name: "X-{{ .Env.MCP_HDR }}".to_string(),
                value: "Bearer {{ .Env.MCP_HDR }}".to_string(),
            }],
        };
    });
    ctx.template_vars_mut().set_env("MCP_HDR", "rendered");

    let mut mcp = ctx.config.mcp.clone();
    super::render_strings(&ctx, &mut mcp).expect("render_strings succeeds");

    let header = &mcp.packages[0].transport.headers[0];
    assert_eq!(
        header.name, "X-{{ .Env.MCP_HDR }}",
        "header name must stay byte-for-byte literal"
    );
    assert_eq!(
        header.value, "Bearer rendered",
        "header value must be template-rendered"
    );
}

// ---------------------------------------------------------------------------
// Preflight requirements
// ---------------------------------------------------------------------------

#[test]
fn requirements_self_gate_on_name_and_skip() {
    use anodizer_core::Publisher as _;
    let publisher = super::publisher::McpPublisher::new();

    let ctx = mcp_ctx(|mcp| {
        mcp.name = None;
    });
    assert!(
        publisher.requirements(&ctx).is_empty(),
        "unset mcp.name must contribute no requirements"
    );

    let ctx = mcp_ctx(|mcp| {
        mcp.skip = Some(StringOrBool::Bool(true));
    });
    assert!(
        publisher.requirements(&ctx).is_empty(),
        "skip: true must contribute no requirements"
    );
}

#[test]
fn requirements_follow_the_auth_method() {
    use anodizer_core::EnvRequirement;
    use anodizer_core::Publisher as _;
    let publisher = super::publisher::McpPublisher::new();

    // github + sole env-ref token: any-of ladder with the fallback var.
    let ctx = mcp_ctx(|mcp| {
        mcp.auth.method = McpAuthMethod::Github;
        mcp.auth.token = "{{ .Env.MCP_PAT }}".to_string();
    });
    assert_eq!(
        publisher.requirements(&ctx),
        vec![EnvRequirement::EnvAnyOf {
            vars: vec!["MCP_PAT".to_string(), "MCP_GITHUB_TOKEN".to_string()],
        }]
    );

    // github + unset token: the env fallback is the only source.
    let ctx = mcp_ctx(|mcp| {
        mcp.auth.method = McpAuthMethod::Github;
        mcp.auth.token = String::new();
    });
    assert_eq!(
        publisher.requirements(&ctx),
        vec![EnvRequirement::EnvAllOf {
            vars: vec!["MCP_GITHUB_TOKEN".to_string()],
        }]
    );

    // github-oidc: both Actions runtime vars, token ignored.
    let ctx = mcp_ctx(|mcp| {
        mcp.auth.method = McpAuthMethod::GithubOidc;
    });
    assert_eq!(
        publisher.requirements(&ctx),
        vec![EnvRequirement::EnvAllOf {
            vars: vec![
                "ACTIONS_ID_TOKEN_REQUEST_URL".to_string(),
                "ACTIONS_ID_TOKEN_REQUEST_TOKEN".to_string(),
            ],
        }]
    );

    // none + literal pre-issued JWT (the mcp_ctx default): nothing to check.
    let ctx = mcp_ctx(|_| {});
    assert!(publisher.requirements(&ctx).is_empty());

    // none + templated JWT: its env refs must be present.
    let ctx = mcp_ctx(|mcp| {
        mcp.auth.token = "{{ .Env.MCP_STATIC_JWT }}".to_string();
    });
    assert_eq!(
        publisher.requirements(&ctx),
        vec![EnvRequirement::EnvAllOf {
            vars: vec!["MCP_STATIC_JWT".to_string()],
        }]
    );
}
