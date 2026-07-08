//! `McpPublisher` — Manager-group `Publisher` impl wrapping the
//! top-level [`publish_to_mcp`](super::publish_to_mcp) entrypoint.
//!
//! MCP is structurally different from krew: krew opens a PR against a
//! GitHub repo (`krew-index`) and the natural rollback is `gh pr close`.
//! MCP POSTs a server JSON document to a **registry API** (POST
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
use anodizer_core::retry::{RetryLog, SuccessClass, http_status, retry_http_blocking};
use anodizer_core::url::percent_encode_unreserved;
use anyhow::Context as _;

use super::McpTarget;
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

/// Outcome of a single-target rollback attempt.
enum RollbackOutcome {
    /// PATCH returned 2xx; the registry has marked the version deleted.
    Restored,
    /// Registry returned 501/403/404; a warn was emitted. Not an error.
    DegradedToWarn,
}

/// Decode the `mcp_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
fn decode_mcp_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<McpTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Mcp(m) => m.mcp_targets.clone(),
        _ => Vec::new(),
    }
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
        .get_token(log)
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
        "statusMessage": format!("anodizer auto-rollback for {}", target.version),
    })
    .to_string();

    let client =
        build_client(Duration::from_secs(60)).context("mcp: build rollback HTTP client")?;

    let result = retry_http_blocking(
        RetryLog::new("mcp: PATCH status", log),
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
                "marked mcp '{}@{}' as deleted on {}",
                target.server_name, target.version, target.registry_url,
            ));
            Ok(RollbackOutcome::Restored)
        }
        Err(err) => {
            let status = http_status(&err);

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
            log.warn(&format!("mcp rollback degraded to warn — {}", reason));
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
        Self::resolved_required(self)
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        use anodizer_core::config::McpAuthMethod;
        let cfg = &ctx.config.mcp;
        if cfg.name.as_deref().unwrap_or("").is_empty()
            || crate::publisher_helpers::entry_inactive(
                ctx,
                cfg.skip.as_ref(),
                None,
                cfg.if_condition.as_deref(),
            )
        {
            return Vec::new();
        }
        match cfg.auth.method {
            // Anonymous publish needs no credential; a templated static-JWT
            // override still declares its env refs so a half-set CI secret
            // fails preflight instead of silently degrading to anonymous.
            McpAuthMethod::None => {
                let refs = anodizer_core::env_preflight::template_env_refs(&cfg.auth.token);
                if refs.is_empty() {
                    Vec::new()
                } else {
                    vec![anodizer_core::EnvRequirement::EnvAllOf { vars: refs }]
                }
            }
            // PAT exchange: rendered `auth.token` with an MCP_GITHUB_TOKEN
            // env fallback when it renders empty — a sole `{{ .Env.X }}`
            // therefore forms an any-of ladder with the fallback.
            McpAuthMethod::Github => {
                if let Some(var) = anodizer_core::env_preflight::sole_env_ref(&cfg.auth.token) {
                    vec![anodizer_core::EnvRequirement::EnvAnyOf {
                        vars: vec![var, "MCP_GITHUB_TOKEN".to_string()],
                    }]
                } else {
                    crate::publisher_helpers::secret_requirement(
                        Some(cfg.auth.token.as_str()),
                        "MCP_GITHUB_TOKEN",
                    )
                    .into_iter()
                    .collect()
                }
            }
            // The OIDC id-token is fetched from the Actions runtime; both
            // request vars are injected by GitHub only when the workflow
            // has `id-token: write`.
            McpAuthMethod::GithubOidc => vec![anodizer_core::EnvRequirement::EnvAllOf {
                vars: vec![
                    "ACTIONS_ID_TOKEN_REQUEST_URL".to_string(),
                    "ACTIONS_ID_TOKEN_REQUEST_TOKEN".to_string(),
                ],
            }],
        }
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        // The publish path returns Some(McpTarget) only on the success
        // path of the POST; dry-run, skip-true, and missing-name all
        // return None so no phantom target ever lands in evidence. A
        // mid-publish failure on the POST surfaces as Err and aborts
        // before evidence assembly — the rollback contract is therefore
        // "if there's a target in evidence, the version exists on the
        // registry," which is what `--rollback-only` relies on.
        let target = super::publish_to_mcp(ctx, &log)?;
        let mut evidence = anodizer_core::PublishEvidence::new("mcp");
        if let Some(t) = target {
            evidence.extra = anodizer_core::PublishEvidenceExtra::Mcp(
                anodizer_core::publish_evidence::McpExtra {
                    mcp_targets: vec![t],
                },
            );
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
                        "failed to mark mcp '{}@{}' as deleted on {}: {:#}; \
                         verify and restore manually via the registry admin UI",
                        t.server_name, t.version, t.registry_url, e
                    ));
                }
            }
        }
        log.status(&format!(
            "mcp rollback marked {} version(s) as deleted, {} degraded-to-warn, {} failure(s)",
            deleted, degraded, failed
        ));
        Ok(())
    }

    /// Validate the registry credential before any irreversible publisher runs
    /// by performing the same auth round-trip the publish path does
    /// (`provider.login()` + `get_token()`). A definitive registry rejection
    /// (401/403) blocks; an indeterminate outcome (missing OIDC context off a
    /// runner, transport error) warns rather than false-blocking a config that
    /// is valid in its real environment.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use anodizer_core::PreflightCheck;
        let mcp_rendered = match super::render_mcp_config(ctx)? {
            super::McpRenderOutcome::Rendered(cfg) => *cfg,
            super::McpRenderOutcome::Skipped(_) => return Ok(PreflightCheck::Pass),
        };
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let registry_url = ctx
            .render_template(super::resolve_registry_url(&mcp_rendered))
            .unwrap_or_else(|_| super::resolve_registry_url(&mcp_rendered).to_string());
        let provider = provider_for(
            mcp_rendered.auth.method,
            &registry_url,
            &mcp_rendered.auth.token,
            &policy,
        );
        let probe = provider
            .login()
            .and_then(|()| provider.get_token(&ctx.logger("preflight")));
        Ok(match probe {
            Ok(_) => PreflightCheck::Pass,
            Err(err) => {
                let status = http_status(&err);
                match status {
                    401 | 403 => PreflightCheck::Blocker(format!(
                        "mcp registry rejected the credential (HTTP {status})"
                    )),
                    _ => {
                        PreflightCheck::Warning(format!("could not verify mcp credential: {err:#}"))
                    }
                }
            }
        })
    }

    fn skips_on_nightly(&self) -> bool {
        // MCP registries accept version overwrites; nightly publishes are allowed.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }
}

#[cfg(test)]
mod publisher_tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::*;
    use anodizer_core::config::{
        Config, HumanDuration, McpAuth, McpAuthMethod, McpConfig, McpPackage, McpRegistryType,
        McpTransport, McpTransportType, RetryConfig,
    };
    use anodizer_core::context::ContextOptions;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::responder::{
        spawn_oneshot_http_responder, spawn_request_capturing_responder,
    };
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    use super::super::{publish_with_registry, warn_once_lock};

    /// Build a context suitable for rollback tests: named server, tight retry
    /// policy, a non-empty pre-issued JWT (so NoneAuthProvider short-circuits),
    /// and the version template variable set.
    fn rollback_ctx(server_name: &str, registry_url: &str) -> Context {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.retry = Some(RetryConfig {
            attempts: 3,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(5)),
            max_elapsed: None,
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

    /// Build a fully-populated context suitable for end-to-end `Publisher::run`
    /// invocations (one OCI package configured, so `build_server_json` produces
    /// a valid payload). `dry_run` controls whether the publish path
    /// short-circuits before any network I/O.
    fn run_ctx(server_name: &str, registry_url: &str, dry_run: bool) -> Context {
        let config = Config {
            project_name: "anodizer".to_string(),
            retry: Some(RetryConfig {
                attempts: 3,
                delay: HumanDuration(Duration::from_millis(1)),
                max_delay: HumanDuration(Duration::from_millis(5)),
                max_elapsed: None,
            }),
            mcp: McpConfig {
                name: Some(server_name.to_string()),
                description: Some("Test server".to_string()),
                packages: vec![McpPackage {
                    registry_type: McpRegistryType::Oci,
                    identifier: "ghcr.io/test/server:v1".to_string(),
                    transport: McpTransport {
                        kind: McpTransportType::Stdio,
                        ..McpTransport::default()
                    },
                }],
                auth: McpAuth {
                    method: McpAuthMethod::None,
                    token: "preissued-jwt".to_string(),
                },
                registry: Some(registry_url.to_string()),
                ..Default::default()
            },
            ..Config::default()
        };
        let opts = ContextOptions {
            dry_run,
            ..ContextOptions::default()
        };
        let mut ctx = Context::new(config, opts);
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
        evidence.extra =
            anodizer_core::PublishEvidenceExtra::Mcp(anodizer_core::publish_evidence::McpExtra {
                mcp_targets: vec![target],
            });
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
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("mcp");
        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("mcp")
                && m.contains("registry publishes")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
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
        let extra =
            anodizer_core::PublishEvidenceExtra::Mcp(anodizer_core::publish_evidence::McpExtra {
                mcp_targets: original.clone(),
            });
        let decoded = decode_mcp_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn mcp_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence with a populated
        // variant and assert (a) no credential-shaped keys appear AND
        // (b) the operator-public registry coordinates are preserved.
        let mut e = PublishEvidence::new("mcp");
        e.extra =
            anodizer_core::PublishEvidenceExtra::Mcp(anodizer_core::publish_evidence::McpExtra {
                mcp_targets: vec![McpTarget {
                    target: "io.github.user/weather".into(),
                    server_name: "io.github.user/weather".into(),
                    registry_url: "https://registry.modelcontextprotocol.io".into(),
                    version: "1.2.3".into(),
                    auth_method: McpAuthMethod::Github,
                }],
            });
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: registry coordinates serialize.
        assert!(s.contains("\"version\":\"1.2.3\""), "{s}");
        assert!(s.contains("\"auth_method\":\"github\""), "{s}");
        assert!(
            s.contains("\"server_name\":\"io.github.user/weather\""),
            "{s}"
        );
        assert!(
            s.contains("\"registry_url\":\"https://registry.modelcontextprotocol.io\""),
            "{s}"
        );
    }

    #[test]
    fn mcp_publish_with_registry_returns_none_when_name_unset() {
        // Missing name short-circuits before the POST; no target produced.
        let _g = warn_once_lock();
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = run_ctx("io.github.user/weather", &registry, false);
        ctx.config.mcp.name = None;

        let log = ctx.logger("mcp-test");
        let target = publish_with_registry(&mut ctx, &log, &registry).expect("ok");
        assert!(target.is_none(), "missing name must not produce a target");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mcp_publish_with_registry_returns_some_on_publish_success() {
        // The success path produces a populated McpTarget reflecting what
        // the registry stored — used by Publisher::run to assemble evidence.
        let _g = warn_once_lock();
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = run_ctx("io.github.user/weather", &registry, false);

        let log = ctx.logger("mcp-test");
        let target = publish_with_registry(&mut ctx, &log, &registry)
            .expect("ok")
            .expect("target present on success");
        assert_eq!(target.server_name, "io.github.user/weather");
        assert_eq!(target.registry_url, registry);
        assert_eq!(target.version, "1.0.0");
        assert_eq!(target.auth_method, McpAuthMethod::None);
    }

    #[test]
    fn mcp_run_writes_no_extra_under_dry_run() {
        // Regression guard for the v0 phantom-evidence bug: under --dry-run
        // the publish path short-circuits without POSTing, so Publisher::run
        // must produce evidence with no `mcp_targets` key (otherwise a later
        // --rollback-only would PATCH a version that was never published).
        let _g = warn_once_lock();
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = run_ctx("io.github.user/weather", &registry, true);

        let p = McpPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run must succeed");
        assert!(
            decode_mcp_targets(&evidence.extra).is_empty(),
            "dry-run must not record any mcp_targets; got {:?}",
            evidence.extra
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "dry-run must not contact the registry"
        );
    }

    #[test]
    fn mcp_run_writes_no_extra_when_skip_evaluates_true() {
        // Same phantom-evidence guard for the skip-template short-circuit.
        let _g = warn_once_lock();
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = run_ctx("io.github.user/weather", &registry, false);
        ctx.config.mcp.skip = Some(anodizer_core::config::StringOrBool::String(
            "{{ true }}".to_string(),
        ));

        let p = McpPublisher::new();
        let evidence = p.run(&mut ctx).expect("skip-true must succeed");
        assert!(
            decode_mcp_targets(&evidence.extra).is_empty(),
            "skip=true must not record any mcp_targets; got {:?}",
            evidence.extra
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mcp_rollback_warns_per_target_when_auth_token_template_invalid() {
        // Token re-render at rollback time can fail (e.g. unclosed tera tag).
        // The per-target failure must be caught and surfaced as a warn so
        // sibling publishers continue rolling back — not propagated as Err.
        let _g = warn_once_lock();
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let registry = format!("http://{addr}");
        let mut ctx = rollback_ctx("io.github.user/weather", &registry);
        // Unclosed tera tag — render_template returns Err.
        ctx.config.mcp.auth.token = "{{ invalid template".to_string();
        let evidence = evidence_for("io.github.user/weather", "1.0.0", &registry);

        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        // Auth re-render failed before any HTTP call; the responder must not
        // have been contacted.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "rollback must not contact the registry when auth re-resolution fails"
        );
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
            max_elapsed: None,
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
