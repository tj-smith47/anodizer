//! `McpPublisher` — Manager-group `Publisher` impl wrapping the
//! top-level [`publish_to_mcp`](super::publish_to_mcp) entrypoint.
//!
//! MCP is structurally different from krew: krew opens a PR against a
//! GitHub repo (`krew-index`) and the natural rollback is `gh pr close`.
//! MCP POSTs an `apiv0.ServerJSON` to a **registry API** (POST
//! `{registry}/v0/publish`); rollback PATCHes the published server-version
//! status to `"deleted"` via `PATCH {registry}/v0/servers/{name}/versions/{version}/status`.
//!
//! Graceful degradation: when the registry returns 501 (status mutation not
//! supported), 403 (insufficient permissions), or 404 (server/version not
//! found), the per-target outcome becomes `DegradedToWarn` — a warn is emitted
//! and rollback continues without propagating an error, so sibling publishers
//! can still roll back.
//!
//! CREDENTIAL HANDLING: [`McpTarget`] stores no auth material. The
//! registry token is re-rendered from `ctx.config.mcp.auth.token` at
//! rollback time via the same template engine the publish path uses. The
//! `auth_method` field captures which provider to use (enum, no secret);
//! the actual credential lives only in the process environment.

use std::time::Duration;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{HttpError, SuccessClass, retry_http_blocking};
use anodizer_core::url::percent_encode_unreserved;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::auth::{build_client, provider_for};

simple_publisher!(
    McpPublisher,
    "mcp",
    anodizer_core::PublisherGroup::Manager,
    false,
    // The PATCH /v0/servers/.../status endpoint uses the same registry JWT
    // that the publish path obtains via the configured auth method.
    // `MCP_GITHUB_TOKEN` is the env var the `github` auth method falls back to;
    // operators need it (or OIDC permissions) for status-mutation rollback.
    Some("MCP_GITHUB_TOKEN status-mutation"),
);

/// Serialized shape of a recorded MCP publish. Single-entry per run —
/// MCP is top-level (one `mcp:` block) — so we still store it as a Vec
/// for shape-parity with the krew/homebrew/scoop targets.
///
/// `server_name` is the rendered `mcp.name` (already template-resolved)
/// and `registry_url` is the resolved endpoint (config override or
/// [`super::manifest::DEFAULT_REGISTRY_URL`]).
///
/// `version` and `auth_method` are captured at publish time so rollback
/// can reconstruct the PATCH URL and re-authenticate without reading config
/// (which might have changed between publish and rollback invocations).
///
/// NB: no `token`, `password`, or `pat` fields — see module rustdoc for
/// the credential-handling rationale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct McpTarget {
    /// Per-target label — duplicates `server_name` for log-line shape
    /// parity with the krew/homebrew/scoop publishers.
    target: String,
    /// Fully-qualified MCP server name in reverse-DNS form
    /// (e.g. `io.github.user/weather`).
    server_name: String,
    /// Resolved registry endpoint the publish path posted to.
    registry_url: String,
    /// Version string published (`ctx.version()` at publish time).
    version: String,
    /// Auth method in use — determines which provider rollback builds.
    /// Stored as the enum (serializes as `"none"` / `"github"` /
    /// `"github-oidc"`) so rollback re-authenticates identically to publish.
    auth_method: anodizer_core::config::McpAuthMethod,
}

/// Outcome of a single-target rollback attempt.
enum RollbackOutcome {
    /// PATCH returned 2xx; the registry has marked the version deleted.
    Restored,
    /// Registry returned 501/403/404; a warn was emitted. Not an error.
    DegradedToWarn,
}

/// Decode the `mcp_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
fn decode_mcp_targets(extra: &serde_json::Value) -> Vec<McpTarget> {
    extra
        .get("mcp_targets")
        .and_then(|v| serde_json::from_value::<Vec<McpTarget>>(v.clone()).ok())
        .unwrap_or_default()
}

/// Resolve the effective registry URL the publisher will POST to.
/// Mirrors the resolution in [`super::publish_to_mcp`].
fn resolve_registry_url(ctx: &Context) -> String {
    ctx.config
        .mcp
        .registry
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(super::manifest::DEFAULT_REGISTRY_URL)
        .to_string()
}

/// Build the single-element target list for this run. Reads the
/// template-rendered server name so the recorded value matches what
/// the registry actually stored. Returns `None` when the skip
/// gate would fire (matches `publish_to_mcp`'s no-op semantics).
fn collect_mcp_run_target(ctx: &Context) -> Option<McpTarget> {
    let mcp = &ctx.config.mcp;
    let raw_name = mcp.name.as_deref().unwrap_or("").trim();
    if raw_name.is_empty() {
        return None;
    }
    // The publish path renders `mcp.name` through the template engine
    // (see super::render_strings); reproduce here so the persisted
    // value matches the wire form. Fall back to the raw string when
    // rendering fails — the publish path itself would have errored on
    // the same template, so the evidence "lies" only in the same
    // failure mode the publish path already surfaces.
    let server_name = ctx
        .render_template(raw_name)
        .unwrap_or_else(|_| raw_name.to_string());
    Some(McpTarget {
        target: server_name.clone(),
        server_name,
        registry_url: resolve_registry_url(ctx),
        version: ctx.version(),
        auth_method: mcp.auth.method,
    })
}

/// PATCH the registry to mark one published server-version as `"deleted"`.
///
/// Re-resolves the auth token at call time by rendering
/// `ctx.config.mcp.auth.token` — so token rotation between publish and
/// rollback surfaces as a per-target warn rather than a hard error.
///
/// Returns `Ok(RollbackOutcome::DegradedToWarn)` when the registry
/// indicates status mutation is unsupported (501), permissions are
/// insufficient (403), or the server/version no longer exists (404). All
/// other errors propagate as `Err`.
fn rollback_one_target(
    ctx: &Context,
    target: &McpTarget,
    log: &StageLogger,
) -> anyhow::Result<RollbackOutcome> {
    // Re-render the token at rollback time; the value is never stored in evidence.
    let rendered_token = ctx
        .render_template(&ctx.config.mcp.auth.token)
        .context("mcp: render auth.token for rollback")?;

    let policy = ctx.retry_policy();
    let provider = provider_for(
        target.auth_method,
        &target.registry_url,
        &rendered_token,
        &policy,
    );
    provider.login().context("mcp: rollback login")?;
    let token = provider
        .get_token()
        .context("mcp: rollback get registry token")?;

    // `percent_encode_unreserved` encodes `/` (it's in the UNRESERVED encode
    // set — RFC 3986 path segments must encode slash), so
    // `io.github.user/weather` → `io.github.user%2Fweather` and
    // `1.0.0+sha.abc` → `1.0.0%2Bsha.abc`, both safe as URL path segments.
    let enc_name = percent_encode_unreserved(&target.server_name);
    let enc_version = percent_encode_unreserved(&target.version);
    let patch_url = format!(
        "{}/v0/servers/{}/versions/{}/status",
        target.registry_url.trim_end_matches('/'),
        enc_name,
        enc_version,
    );

    let body = serde_json::json!({
        "status": "deleted",
        "statusMessage": format!("anodizer auto-rollback for v{}", target.version),
    })
    .to_string();

    let client =
        build_client(Duration::from_secs(60)).context("mcp: build rollback HTTP client")?;

    let result = retry_http_blocking(
        "mcp: PATCH status",
        &policy,
        SuccessClass::Strict,
        |_| {
            client
                .patch(&patch_url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", token))
                .body(body.clone())
                .send()
        },
        |status, body| {
            format!(
                "mcp: PATCH {} returned HTTP {}: {}",
                patch_url,
                status,
                anodizer_core::redact::redact_bearer_tokens(body)
            )
        },
    );

    match result {
        Ok(_) => {
            log.status(&format!(
                "mcp: marked '{}@{}' as deleted on {}",
                target.server_name, target.version, target.registry_url,
            ));
            Ok(RollbackOutcome::Restored)
        }
        Err(err) => {
            // Walk the anyhow chain looking for HttpError to extract the HTTP status.
            let status = err
                .chain()
                .find_map(|e| e.downcast_ref::<HttpError>().map(|h| h.status))
                .unwrap_or(0);

            let reason = match status {
                501 => format!(
                    "registry at {} does not support status mutation (HTTP 501)",
                    target.registry_url
                ),
                403 => format!(
                    "insufficient permissions to mark '{}@{}' as deleted on {} (HTTP 403)",
                    target.server_name, target.version, target.registry_url
                ),
                404 => format!(
                    "server version '{}@{}' not found on {} (HTTP 404; already deleted?)",
                    target.server_name, target.version, target.registry_url
                ),
                _ => return Err(err),
            };
            log.warn(&format!("mcp: rollback degraded to warn: {}", reason));
            Ok(RollbackOutcome::DegradedToWarn)
        }
    }
}

impl anodizer_core::Publisher for McpPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        Self::PUBLISHER_REQUIRED
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        // Snapshot the target shape BEFORE the publish path runs, so
        // even a mid-publish failure on the POST leaves the operator
        // a hint of what was attempted. (The publish path is
        // idempotent over the server_name+version coordinates, so a
        // resurrected re-publish hitting the same name is the
        // expected recovery path.)
        let target = collect_mcp_run_target(ctx);
        super::publish_to_mcp(ctx, &log)?;
        let mut evidence = anodizer_core::PublishEvidence::new("mcp");
        if let Some(t) = target {
            evidence.extra = serde_json::json!({ "mcp_targets": [t] });
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_mcp_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "mcp",
                "registry publishes",
            ));
            return Ok(());
        }
        let mut deleted = 0usize;
        let mut degraded = 0usize;
        let mut failed = 0usize;
        for t in &targets {
            match rollback_one_target(ctx, t, &log) {
                Ok(RollbackOutcome::Restored) => deleted += 1,
                Ok(RollbackOutcome::DegradedToWarn) => degraded += 1,
                Err(e) => {
                    failed += 1;
                    log.warn(&format!(
                        "mcp: failed to mark '{}@{}' as deleted on {}: {:#}; \
                         verify and restore manually via the registry admin UI",
                        t.server_name, t.version, t.registry_url, e
                    ));
                }
            }
        }
        log.status(&format!(
            "mcp: rollback marked {} version(s) as deleted, {} degraded-to-warn, {} failure(s)",
            deleted, degraded, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

#[cfg(test)]
mod publisher_tests {
    use std::time::Duration;

    use super::*;
    use anodizer_core::config::{HumanDuration, McpAuth, McpAuthMethod, McpConfig, RetryConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::responder::{
        spawn_oneshot_http_responder, spawn_request_capturing_responder,
    };
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn mcp_ctx_named(server_name: &str) -> Context {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.mcp = McpConfig {
            name: Some(server_name.to_string()),
            ..Default::default()
        };
        ctx
    }

    /// Build a context suitable for rollback tests: named server, tight retry
    /// policy, a non-empty pre-issued JWT (so NoneAuthProvider short-circuits),
    /// and the version template variable set.
    fn rollback_ctx(server_name: &str, registry_url: &str) -> Context {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.retry = Some(RetryConfig {
            attempts: 3,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(5)),
        });
        ctx.config.mcp = McpConfig {
            name: Some(server_name.to_string()),
            auth: McpAuth {
                method: McpAuthMethod::None,
                token: "preissued-jwt".to_string(),
            },
            registry: Some(registry_url.to_string()),
            ..Default::default()
        };
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx
    }

    /// Build a `PublishEvidence` with one `McpTarget` pointing at the given
    /// registry. Used by rollback tests to bypass the publish path entirely.
    fn evidence_for(server_name: &str, version: &str, registry_url: &str) -> PublishEvidence {
        let target = McpTarget {
            target: server_name.to_string(),
            server_name: server_name.to_string(),
            registry_url: registry_url.to_string(),
            version: version.to_string(),
            auth_method: McpAuthMethod::None,
        };
        let mut evidence = PublishEvidence::new("mcp");
        evidence.extra = serde_json::json!({ "mcp_targets": [target] });
        evidence
    }

    #[test]
    fn mcp_publisher_classification() {
        let p = McpPublisher::new();
        assert_eq!(p.name(), "mcp");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("MCP_GITHUB_TOKEN status-mutation")
        );
    }

    #[test]
    fn mcp_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = McpPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn mcp_rollback_warns_when_no_targets_recorded() {
        let mut ctx = TestContextBuilder::new().build();
        let evidence = PublishEvidence::new("mcp");
        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let msg = crate::publisher_helpers::rollback_empty_warning_msg("mcp", "registry publishes");
        assert!(msg.starts_with("mcp:"), "{msg}");
        assert!(msg.contains("registry publishes"), "{msg}");
        assert!(msg.contains("verify"), "{msg}");
        assert!(msg.contains("manually"), "{msg}");
    }

    #[test]
    fn mcp_target_extra_roundtrips() {
        let original = vec![McpTarget {
            target: "io.github.user/weather".into(),
            server_name: "io.github.user/weather".into(),
            registry_url: "https://registry.modelcontextprotocol.io".into(),
            version: "1.2.3".into(),
            auth_method: McpAuthMethod::Github,
        }];
        let extra = serde_json::json!({ "mcp_targets": original.clone() });
        let decoded = decode_mcp_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn mcp_target_extra_carries_no_secret_material() {
        let t = McpTarget {
            target: "io.github.user/weather".into(),
            server_name: "io.github.user/weather".into(),
            registry_url: "https://registry.modelcontextprotocol.io".into(),
            version: "1.2.3".into(),
            auth_method: McpAuthMethod::Github,
        };
        let s = serde_json::to_string(&t).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        // New fields are present and carry no secret values.
        assert!(
            s.contains("\"version\":"),
            "version field must be present: {s}"
        );
        assert!(
            s.contains("\"auth_method\":"),
            "auth_method field must be present: {s}"
        );
    }

    #[test]
    fn mcp_collect_run_target_skip_when_name_unset() {
        let ctx = TestContextBuilder::new().build();
        assert!(collect_mcp_run_target(&ctx).is_none());
    }

    #[test]
    fn mcp_collect_run_target_captures_default_registry() {
        let mut ctx = mcp_ctx_named("io.github.user/weather");
        ctx.template_vars_mut().set("Version", "2.0.0");
        let t = collect_mcp_run_target(&ctx).expect("target");
        assert_eq!(t.server_name, "io.github.user/weather");
        assert_eq!(t.registry_url, super::super::manifest::DEFAULT_REGISTRY_URL);
        assert_eq!(t.version, "2.0.0");
        assert_eq!(t.auth_method, McpAuthMethod::None);
    }

    #[test]
    fn mcp_collect_run_target_captures_registry_override() {
        let mut ctx = mcp_ctx_named("io.github.user/weather");
        ctx.config.mcp.registry = Some("https://staging.example.com".to_string());
        let t = collect_mcp_run_target(&ctx).expect("target");
        assert_eq!(t.registry_url, "https://staging.example.com");
    }

    #[test]
    fn mcp_collect_run_target_captures_version_from_context() {
        let mut ctx = mcp_ctx_named("io.github.user/weather");
        ctx.template_vars_mut().set("Version", "3.1.4");
        let t = collect_mcp_run_target(&ctx).expect("target");
        assert_eq!(t.version, "3.1.4");
    }

    // ---------------------------------------------------------------------------
    // Rollback HTTP tests
    // ---------------------------------------------------------------------------

    #[test]
    fn mcp_rollback_patches_status_to_deleted_with_responder() {
        let (addr, calls) =
            spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"]);
        let registry = format!("http://{addr}");
        let mut ctx = rollback_ctx("io.github.user/weather", &registry);
        let evidence = evidence_for("io.github.user/weather", "1.0.0", &registry);

        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn mcp_rollback_degrades_to_warn_on_501() {
        // retry_http_blocking retries 5xx, so we need enough 501s to exhaust
        // the 3-attempt budget.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\n\r\n",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = rollback_ctx("io.github.user/weather", &registry);
        let evidence = evidence_for("io.github.user/weather", "1.0.0", &registry);

        let p = McpPublisher::new();
        // Must not return Err; 501 degrades to warn.
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        // All 3 attempts were consumed by the retry budget.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn mcp_rollback_degrades_to_warn_on_403() {
        // 4xx fast-fails after one attempt.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = rollback_ctx("io.github.user/weather", &registry);
        let evidence = evidence_for("io.github.user/weather", "1.0.0", &registry);

        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn mcp_rollback_degrades_to_warn_on_404() {
        // 404 fast-fails after one attempt.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = rollback_ctx("io.github.user/weather", &registry);
        let evidence = evidence_for("io.github.user/weather", "1.0.0", &registry);

        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn mcp_rollback_url_encodes_server_name_and_version() {
        // server_name contains `/` (must become %2F) and version contains `+`
        // (must become %2B) — both are outside the RFC 3986 unreserved set
        // and must not appear as literal characters in the URL path segment.
        let (addr, captured) =
            spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        let registry = format!("http://{addr}");
        let mut ctx = rollback_ctx("io.github.user/weather", &registry);
        ctx.config.retry = Some(RetryConfig {
            attempts: 1,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(5)),
        });

        let evidence = evidence_for("io.github.user/weather", "1.0.0+sha.abc", &registry);
        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let raw = captured.lock().unwrap().clone();
        // The request line must contain the percent-encoded forms.
        assert!(
            raw.contains("io.github.user%2Fweather"),
            "server_name '/' must be encoded as %2F; got: {raw}"
        );
        assert!(
            raw.contains("1.0.0%2Bsha.abc"),
            "version '+' must be encoded as %2B; got: {raw}"
        );
        // The literal slash must not appear in the path segment (only the
        // fixed structural slashes between /v0/servers/, /versions/, /status).
        // The URL path structure is /v0/servers/<name>/versions/<version>/status.
        // After /v0/servers/ there must not be an unencoded `/` before /versions/.
        assert!(
            !raw.contains("/io.github.user/weather/"),
            "unencoded slash must not appear in path segment: {raw}"
        );
    }
}
