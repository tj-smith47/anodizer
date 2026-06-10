//! MCP `server.json` schema validation.
//!
//! The MCP registry validates every published server document against its
//! `server.schema.json` (a draft-07 schema pinning the reverse-DNS `name`
//! pattern, the `registryType` / `transport` package shape, the
//! required `name`/`description`/`version` set, and the `repository`
//! `url`+`source` requirement). anodizer renders ONE such document per
//! release from the top-level `mcp:` block — there is no per-crate manifest.
//! This validator renders the exact `server.json` the live publish would POST,
//! via the same [`crate::mcp::render_server_json`] pipeline, and checks it
//! against the vendored schema so a structural defect (an out-of-pattern
//! `name`, a missing required field, a wrong-typed value) surfaces in the
//! snapshot/dry-run pass rather than after a real release has uploaded it.
//!
//! Unlike the winget / scoop / krew validators, MCP collects no build
//! artifacts and runs no per-crate loop: the document references npm / oci /
//! pypi packages by name (not by a built archive), and its version is the
//! single global [`anodizer_core::context::Context::version`]. There is thus
//! no shard-tolerance / artifact-eligibility gate — nothing the validator
//! could skip on a single-target snapshot, because no built artifact gates the
//! render.

use anodizer_core::context::Context;
use anyhow::{Context as _, Result};

use super::{PublisherSchemaValidator, SchemaFinding, TagResolver, validate_json};

/// The MCP registry's vendored `server.json` schema (draft-07). Pinned to the
/// `$schema` version anodizer stamps onto every published document
/// (`CURRENT_SCHEMA_URL` in `mcp/manifest.rs`) and embedded so validation is
/// fully offline; refresh via `schemas/SOURCES.md`.
const MCP_SCHEMA: &str = include_str!("../../schemas/mcp.server.2025-12-11.schema.json");

/// Validates anodizer's rendered MCP `server.json` against the registry's
/// published JSON Schema.
pub(crate) struct McpSchemaValidator;

impl PublisherSchemaValidator for McpSchemaValidator {
    fn publisher(&self) -> &'static str {
        "mcp"
    }

    fn validate(
        &self,
        ctx: &mut Context,
        _resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>> {
        // MCP is a single project-wide `server.json` (not per-crate), so it
        // renders under the global version the stage scoped — no per-crate
        // re-scope is needed; `_resolve_tag` is unused for this publisher.
        //
        // `None` means the publisher is unconfigured / disabled (no `mcp.name`,
        // truthy `mcp.skip`, or falsy `mcp.if`) — nothing to render or validate.
        let Some(server) = crate::mcp::render_server_json(ctx)? else {
            return Ok(vec![]);
        };
        let value = serde_json::to_value(&server)
            .context("mcp: serialize rendered server.json for schema validation")?;
        validate_json("mcp", &value, MCP_SCHEMA)
    }
}

#[cfg(test)]
mod tests {
    use anodizer_core::config::{
        McpAuth, McpAuthMethod, McpConfig, McpHeader, McpPackage, McpRegistryType, McpRepository,
        McpTransport, McpTransportType, ReleaseConfig, ScmRepoConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;
    use serde_json::Value;

    use super::*;

    /// An `McpConfig` exercising every server-document-affecting option, with
    /// values the MCP schema accepts: a reverse-DNS `name` matching the
    /// `^…/…$` pattern, a non-empty description/title under the 100-char cap, a
    /// populated repository, and one package per non-stdio-incompatible
    /// registry type. These transports are `stdio` (no `url`); remote
    /// transports carrying a `url` + `headers` are exercised separately by
    /// [`remote_transport_validates_and_emits_url_and_headers`].
    fn every_option_mcp_cfg() -> McpConfig {
        McpConfig {
            name: Some("io.github.acme/widget".to_string()),
            title: Some("Widget".to_string()),
            description: Some("A widget management MCP server".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            packages: vec![
                McpPackage {
                    registry_type: McpRegistryType::Npm,
                    identifier: "@acme/widget".to_string(),
                    transport: McpTransport {
                        kind: McpTransportType::Stdio,
                        ..McpTransport::default()
                    },
                },
                McpPackage {
                    registry_type: McpRegistryType::Oci,
                    identifier: "ghcr.io/acme/widget:v{{ .Version }}".to_string(),
                    transport: McpTransport {
                        kind: McpTransportType::Stdio,
                        ..McpTransport::default()
                    },
                },
                McpPackage {
                    registry_type: McpRegistryType::Pypi,
                    identifier: "acme-widget".to_string(),
                    transport: McpTransport {
                        kind: McpTransportType::Stdio,
                        ..McpTransport::default()
                    },
                },
            ],
            transports: vec![],
            skip: None,
            repository: McpRepository {
                url: "https://github.com/acme/widget".to_string(),
                source: "github".to_string(),
                id: "r-123".to_string(),
                subfolder: "crates/widget".to_string(),
            },
            auth: McpAuth {
                method: McpAuthMethod::GithubOidc,
                token: String::new(),
            },
            registry: Some("https://registry.example.test".to_string()),
            required: Some(true),
            if_condition: None,
            retain_on_rollback: None,
        }
    }

    /// Build a context whose top-level `mcp:` block is `cfg`, a `release.github`
    /// block (so repository inference has a source to fall back to), and the
    /// `Version` template var scoped to `version` — exactly the shape the
    /// publish stage scopes before invoking the MCP publisher.
    fn ctx_with_mcp(cfg: McpConfig, version: &str) -> Context {
        let mut ctx = TestContextBuilder::new().snapshot(true).build();
        ctx.config.mcp = cfg;
        ctx.config.release = Some(ReleaseConfig {
            github: Some(ScmRepoConfig {
                owner: "acme".to_string(),
                name: "widget".to_string(),
            }),
            ..Default::default()
        });
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
        ctx
    }

    /// The every-option `server.json` must conform with zero findings AND land
    /// each option where the registry schema expects it.
    #[test]
    fn every_option_validates_and_lands_in_fields() {
        let mut ctx = ctx_with_mcp(every_option_mcp_cfg(), "1.0.0");

        let findings = McpSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option server.json must conform, got: {findings:?}"
        );

        let server = crate::mcp::render_server_json(&ctx)
            .expect("render ok")
            .expect("not skipped");
        let v: Value = serde_json::to_value(&server).expect("serialize");

        assert_eq!(
            v["$schema"],
            "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json"
        );
        assert_eq!(v["name"], "io.github.acme/widget");
        assert_eq!(v["title"], "Widget");
        assert_eq!(v["description"], "A widget management MCP server");
        assert_eq!(v["version"], "1.0.0");
        assert_eq!(v["websiteUrl"], "https://acme.example/widget");
        assert_eq!(v["repository"]["url"], "https://github.com/acme/widget");
        assert_eq!(v["repository"]["source"], "github");

        // The npm package carries the global version; its registryType +
        // identifier + transport land where the schema's Package def expects.
        let npm = &v["packages"][0];
        assert_eq!(npm["registryType"], "npm");
        assert_eq!(npm["identifier"], "@acme/widget");
        assert_eq!(npm["version"], "1.0.0");
        assert_eq!(npm["transport"]["type"], "stdio");

        // The OCI package pins its version in the `:tag` of the templated
        // identifier, so the redundant `version` field is omitted on the wire.
        let oci = &v["packages"][1];
        assert_eq!(oci["registryType"], "oci");
        assert_eq!(oci["identifier"], "ghcr.io/acme/widget:v1.0.0");
        assert!(
            oci.get("version").is_none(),
            "OCI package omits the redundant version field, got: {oci}"
        );
    }

    /// MCP publishes a single top-level `server.json` keyed by the global
    /// `Version` — there is no per-crate manifest, so the only ctx dimension
    /// that changes the rendered document is what `Version` resolves to. In
    /// single-crate / workspace-lockstep mode every publisher sees one shared
    /// global version; in workspace per-crate mode the publish stage scopes
    /// `Version` to the selected crate's value before invoking the publisher.
    /// Both reduce to "render under the version the stage scoped" — exercised
    /// here as two distinct versions that must each stamp the document and
    /// validate. Asserting three identical copies under fabricated per-crate
    /// configs would prove nothing the version axis does not already cover.
    #[test]
    fn version_axis_drives_the_document_and_validates() {
        for version in ["1.0.0", "2.3.4-alpha"] {
            let mut ctx = ctx_with_mcp(every_option_mcp_cfg(), version);

            let findings = McpSchemaValidator
                .validate(
                    &mut ctx,
                    &crate::schema_validation::test_current_version_resolver(),
                )
                .expect("validation runs");
            assert!(
                findings.is_empty(),
                "server.json must conform under version {version}, got: {findings:?}"
            );

            let server = crate::mcp::render_server_json(&ctx)
                .expect("render ok")
                .expect("not skipped");
            assert_eq!(
                server.version, version,
                "the global Version stamps the document"
            );
            // The OCI identifier's `{{ .Version }}` renders to the same scoped
            // version, so a per-crate-scoped run names the same image tag.
            assert_eq!(
                server.packages[1].identifier,
                format!("ghcr.io/acme/widget:v{version}")
            );
        }
    }

    /// An unconfigured publisher (no `mcp.name`) produces no document and thus
    /// no findings — the validator must not fabricate a violation for a block
    /// the live publish would skip.
    #[test]
    fn unconfigured_mcp_yields_no_findings() {
        let mut ctx = ctx_with_mcp(McpConfig::default(), "1.0.0");
        let findings = McpSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "an unconfigured mcp block yields no findings, got: {findings:?}"
        );
        assert!(
            crate::mcp::render_server_json(&ctx)
                .expect("render ok")
                .is_none(),
            "no document is rendered for an unconfigured block"
        );
    }

    /// A `server.json` whose `name` violates the registry's reverse-DNS pattern
    /// (no `/` separating namespace from server name) is genuinely rejected by
    /// the vendored schema — the finding must name the offending `/name` field.
    /// Proves the validation is non-vacuous: the schema's `pattern` constraint
    /// bites a real defect anodizer could otherwise ship.
    #[test]
    fn name_pattern_violation_is_reported() {
        let server = crate::mcp::render_server_json(&ctx_with_mcp(every_option_mcp_cfg(), "1.0.0"))
            .expect("render ok")
            .expect("not skipped");
        let mut v: Value = serde_json::to_value(&server).expect("serialize");
        // No forward slash — fails the `^[…]+/[…]+$` reverse-DNS pattern.
        v["name"] = Value::String("not-reverse-dns".to_string());

        let findings = validate_json("mcp", &v, MCP_SCHEMA).expect("validation runs");
        assert!(
            findings.iter().any(|f| f.field == "/name"),
            "the name-pattern violation must be reported at /name, got: {findings:?}"
        );
    }

    /// A package with an unknown `registryType` is rejected: the registry's
    /// `Package.transport` anyOf cannot satisfy an out-of-shape package, and a
    /// missing required `version` on the document trips the root `required`
    /// set. Two distinct structural defects, both surfaced — proving the schema
    /// rejects more than just the `name` pattern.
    #[test]
    fn missing_required_version_is_reported_at_root() {
        let server = crate::mcp::render_server_json(&ctx_with_mcp(every_option_mcp_cfg(), "1.0.0"))
            .expect("render ok")
            .expect("not skipped");
        let mut v: Value = serde_json::to_value(&server).expect("serialize");
        v.as_object_mut().expect("object").remove("version");

        let findings = validate_json("mcp", &v, MCP_SCHEMA).expect("validation runs");
        let root = findings
            .iter()
            .find(|f| f.field == "(root)")
            .unwrap_or_else(|| panic!("a root finding for the missing version, got: {findings:?}"));
        assert!(
            root.expected.contains("version"),
            "the finding names the missing required key, got: {}",
            root.expected
        );
    }

    /// A package transport whose `type` is not one of the registry's enum
    /// values is rejected — the emitter would only produce this if a new
    /// `McpTransportType` variant outran the schema, so the guard catches an
    /// enum drift before a real publish.
    #[test]
    fn invalid_package_transport_type_is_reported() {
        let server = crate::mcp::render_server_json(&ctx_with_mcp(every_option_mcp_cfg(), "1.0.0"))
            .expect("render ok")
            .expect("not skipped");
        let mut v: Value = serde_json::to_value(&server).expect("serialize");
        v["packages"][0]["transport"]["type"] = Value::String("carrier-pigeon".to_string());

        let findings = validate_json("mcp", &v, MCP_SCHEMA).expect("validation runs");
        assert!(
            findings
                .iter()
                .any(|f| f.field.starts_with("/packages/0/transport")),
            "the bad transport type must be reported under the package transport, got: {findings:?}"
        );
    }

    /// The vendored schema must stay in lockstep with the `$schema` URL
    /// anodizer stamps onto every document. A future schema bump that forgets to
    /// re-vendor (or vice-versa) would validate against the wrong contract; this
    /// asserts the date that the rendered document's `$schema` carries matches
    /// both the vendored schema's `$id` and the embedded `MCP_SCHEMA` text, so
    /// the desync is impossible to ship.
    #[test]
    fn schema_version_lockstep() {
        // The date the renderer stamps, read end-to-end from a rendered
        // document's `$schema` (the value comes from `CURRENT_SCHEMA_URL`).
        let server = crate::mcp::render_server_json(&ctx_with_mcp(every_option_mcp_cfg(), "1.0.0"))
            .expect("render ok")
            .expect("not skipped");
        let stamped = serde_json::to_value(&server).expect("serialize")["$schema"]
            .as_str()
            .expect("$schema is a string")
            .to_string();

        let schema: Value = serde_json::from_str(MCP_SCHEMA).expect("vendored schema parses");
        let id = schema["$id"].as_str().expect("schema $id is a string");

        // The emitter and the vendored asset must agree on the schema URL
        // verbatim — same version, same path.
        assert_eq!(
            stamped, id,
            "the stamped $schema must equal the vendored schema's $id"
        );

        // And the embedded asset must be the file whose name carries that same
        // version date, so renaming the vendored file or bumping the constant
        // without re-vendoring fails this test.
        let date = id
            .rsplit('/')
            .nth(1)
            .expect("schema $id has a version path segment");
        assert!(
            MCP_SCHEMA.contains(date),
            "the embedded MCP_SCHEMA must be the {date} vendored file"
        );
    }

    /// A `streamable-http` package transport with a templated `url` + header
    /// must (a) render the templates against `.Env`, (b) validate with zero
    /// findings, and (c) emit `type` + `url` + the `{name, value}` headers in
    /// the schema-expected shape. Locks the remote-transport contract end to
    /// end.
    #[test]
    fn remote_transport_validates_and_emits_url_and_headers() {
        let mut cfg = every_option_mcp_cfg();
        cfg.packages = vec![McpPackage {
            registry_type: McpRegistryType::Npm,
            identifier: "@acme/widget".to_string(),
            transport: McpTransport {
                kind: McpTransportType::StreamableHttp,
                url: "https://{{ .Env.MCP_HOST }}/mcp".to_string(),
                headers: vec![McpHeader {
                    name: "Authorization".to_string(),
                    value: "Bearer {{ .Env.MCP_TOKEN }}".to_string(),
                }],
            },
        }];

        let mut ctx = ctx_with_mcp(cfg, "1.0.0");
        ctx.template_vars_mut()
            .set_env("MCP_HOST", "mcp.acme.example");
        ctx.template_vars_mut().set_env("MCP_TOKEN", "s3cr3t");

        let findings = McpSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "a remote-transport server.json must conform, got: {findings:?}"
        );

        let server = crate::mcp::render_server_json(&ctx)
            .expect("render ok")
            .expect("not skipped");
        let v: Value = serde_json::to_value(&server).expect("serialize");
        let transport = &v["packages"][0]["transport"];
        assert_eq!(transport["type"], "streamable-http");
        assert_eq!(
            transport["url"], "https://mcp.acme.example/mcp",
            "the url template must resolve against .Env"
        );
        assert_eq!(transport["headers"][0]["name"], "Authorization");
        assert_eq!(
            transport["headers"][0]["value"], "Bearer s3cr3t",
            "the header value template must resolve against .Env"
        );
    }

    /// A remote (`streamable-http` / `sse`) transport with an EMPTY `url` is a
    /// schema violation: the registry's remote subschemas require `url`. The
    /// emitter drops the empty `url` key (the stdio shape), so the document
    /// fails the `streamable-http`/`sse` `required: [type, url]` branch — the
    /// finding must land under the package transport. Locks the contract that
    /// a misconfigured remote transport is caught before a publish.
    #[test]
    fn remote_transport_without_url_is_reported() {
        let mut cfg = every_option_mcp_cfg();
        cfg.packages = vec![McpPackage {
            registry_type: McpRegistryType::Npm,
            identifier: "@acme/widget".to_string(),
            transport: McpTransport {
                kind: McpTransportType::StreamableHttp,
                url: String::new(),
                headers: vec![],
            },
        }];

        let server = crate::mcp::render_server_json(&ctx_with_mcp(cfg, "1.0.0"))
            .expect("render ok")
            .expect("not skipped");
        let v: Value = serde_json::to_value(&server).expect("serialize");
        assert!(
            v["packages"][0]["transport"].get("url").is_none(),
            "an empty url is dropped from the wire, leaving the remote transport invalid"
        );

        let findings = validate_json("mcp", &v, MCP_SCHEMA).expect("validation runs");
        assert!(
            findings
                .iter()
                .any(|f| f.field.starts_with("/packages/0/transport")),
            "a remote transport missing its required url must be flagged under the package transport, got: {findings:?}"
        );
    }
}
