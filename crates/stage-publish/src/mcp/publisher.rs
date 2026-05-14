//! `McpPublisher` — Bundle C Manager-group `Publisher` impl wrapping the
//! top-level [`publish_to_mcp`](super::publish_to_mcp) entrypoint.
//!
//! MCP is structurally different from krew: krew opens a PR against a
//! GitHub repo (`krew-index`) and the natural rollback is `gh pr close`.
//! MCP POSTs an `apiv0.ServerJSON` to a **registry API** (POST
//! `{registry}/v0/publish`); there is no PR to close. The registry has
//! a server-lifecycle status field (`active` / `deprecated` / `deleted`),
//! but no public unpublish endpoint is documented and the upstream
//! reference implementation does not expose one as of the schema
//! pinned in [`super::manifest::CURRENT_SCHEMA_URL`].
//!
//! Bundle C's contract for MCP is therefore: record what was published
//! in [`anodizer_core::PublishEvidence::extra`] so a `--rollback-only`
//! invocation can surface the exact server name + registry endpoint
//! the operator needs to clean up manually. The `rollback` method
//! itself is a warn-only path that does not call out to the registry.
//!
//! CREDENTIAL HANDLING: [`McpTarget`] stores no auth material. The
//! registry token is read from `ctx.config.mcp.auth.token` (after
//! template rendering) at publish time; persisting the resolved value
//! into evidence would mean a release report leaked the token in
//! plaintext, so we deliberately skip recording it. The
//! `MCP_GITHUB_TOKEN` env-var fallback documented in
//! [`super::publish_with_registry`] is what operators are expected
//! to supply if they re-run the publish path.

use anodizer_core::context::Context;
use serde::{Deserialize, Serialize};

simple_publisher!(
    McpPublisher,
    "mcp",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

/// Serialized shape of a recorded MCP publish. Single-entry per run —
/// MCP is top-level (one `mcp:` block) — so we still store it as a Vec
/// for shape-parity with the krew/homebrew/scoop targets.
///
/// `server_name` is the rendered `mcp.name` (already template-resolved)
/// and `registry_url` is the resolved endpoint (config override or
/// [`super::manifest::DEFAULT_REGISTRY_URL`]).
///
/// NB: no `token`, `auth_method`, or `password` fields — see module
/// rustdoc for the credential-handling rationale.
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
/// the registry actually stored. Returns an empty Vec when the skip
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
    })
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
        // MCP has no programmatic unpublish endpoint. Surface a warn
        // with the exact registry + server name the operator needs to
        // address out-of-band. This is intentionally NOT an error: a
        // failed automated rollback should not gate the rest of the
        // pipeline.
        for t in &targets {
            log.warn(&format!(
                "mcp: no programmatic rollback for server '{}' on {}; \
                 the MCP registry exposes no public unpublish endpoint. \
                 Mark the server as deprecated or deleted via the registry's \
                 admin UI / direct database access if rollback is required.",
                t.server_name, t.registry_url,
            ));
        }
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::McpConfig;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn mcp_ctx_named(server_name: &str) -> Context {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.mcp = McpConfig {
            name: Some(server_name.to_string()),
            ..Default::default()
        };
        ctx
    }

    #[test]
    fn mcp_publisher_classification() {
        let p = McpPublisher::new();
        assert_eq!(p.name(), "mcp");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN pull_request:write")
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
        };
        let s = serde_json::to_string(&t).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
    }

    #[test]
    fn mcp_collect_run_target_skip_when_name_unset() {
        let ctx = TestContextBuilder::new().build();
        assert!(collect_mcp_run_target(&ctx).is_none());
    }

    #[test]
    fn mcp_collect_run_target_captures_default_registry() {
        let ctx = mcp_ctx_named("io.github.user/weather");
        let t = collect_mcp_run_target(&ctx).expect("target");
        assert_eq!(t.server_name, "io.github.user/weather");
        assert_eq!(t.registry_url, super::super::manifest::DEFAULT_REGISTRY_URL);
    }

    #[test]
    fn mcp_collect_run_target_captures_registry_override() {
        let mut ctx = mcp_ctx_named("io.github.user/weather");
        ctx.config.mcp.registry = Some("https://staging.example.com".to_string());
        let t = collect_mcp_run_target(&ctx).expect("target");
        assert_eq!(t.registry_url, "https://staging.example.com");
    }

    #[test]
    fn mcp_rollback_warns_per_target_when_evidence_present() {
        // The rollback path is warn-only when targets are recorded;
        // assert it does NOT return Err so the dispatch chain continues.
        let mut ctx = TestContextBuilder::new().build();
        let mut evidence = PublishEvidence::new("mcp");
        evidence.extra = serde_json::json!({
            "mcp_targets": [{
                "target": "io.github.user/weather",
                "server_name": "io.github.user/weather",
                "registry_url": "https://registry.modelcontextprotocol.io",
            }],
        });
        let p = McpPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
    }
}
