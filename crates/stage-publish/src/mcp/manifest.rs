//! Serde-shaped mirror of `apiv0.ServerJSON` / `model.Package` /
//! `model.Repository` / `model.Transport` from
//! `github.com/modelcontextprotocol/registry/pkg/{api/v0,model}`.
//!
//! The wire format is JSON; field renames preserve the upstream JSON keys
//! (camelCase + `$schema`). `skip_serializing_if = "Option::is_none"` and
//! `Vec::is_empty` mirror the `omitempty` annotations on the Go side so a
//! minimal config round-trips to the same payload Go's `encoding/json`
//! would emit. The corresponding upstream constant is
//! `model.CurrentSchemaURL` (currently
//! `https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json`).

use serde::{Deserialize, Serialize};

/// Current MCP server.json schema URL — sourced from
/// `github.com/modelcontextprotocol/registry/pkg/model/constants.go`
/// (`CurrentSchemaURL`). The schema version string MUST be kept in sync
/// with the upstream registry when it bumps; bumping here is a payload
/// behaviour change so it gets a separate commit.
pub const CURRENT_SCHEMA_URL: &str =
    "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";

/// Default MCP registry base URL — sourced from
/// `github.com/modelcontextprotocol/registry/cmd/publisher/commands/login.go`
/// (`DefaultRegistryURL`).
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.modelcontextprotocol.io";

/// `apiv0.ServerJSON` mirror — the payload POSTed to `/v0/publish`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerJson {
    /// JSON Schema URI for this server.json format (required).
    #[serde(rename = "$schema")]
    pub schema: String,
    /// Server name in reverse-DNS format (required).
    pub name: String,
    /// Clear human-readable description (required by upstream schema; we
    /// allow empty since GR's mcp.go does not enforce non-empty here either).
    pub description: String,
    /// Optional human-readable title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Optional source-repository metadata. Omitted when GR's
    /// `mcp.Repository.URL == ""` — mirror that exact gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    /// Version string for this server. Required (≥1 char). Anodizer sets
    /// this from `ctx.version()` at publish time.
    pub version: String,
    /// Optional homepage / documentation URL. JSON key is `websiteUrl`
    /// (camelCase, single 'L') — pinned from upstream
    /// `apiv0/types.go:43`.
    #[serde(
        rename = "websiteUrl",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub website_url: String,
    /// Distribution packages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<Package>,
}

/// `model.Repository` mirror.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Repository {
    /// Repository URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Repository hosting service identifier (`github`, `gitlab`, `gitea`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Stable hosting-service repository ID.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Optional relative path from repo root to the server location.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subfolder: String,
}

/// `model.Package` mirror. Anodizer only fills the small subset of fields
/// GR's MCP pipe ever populates — `RegistryType`, `Identifier`, `Version`,
/// `Transport` — matching `mcp.go::Publish`'s loop body. Other Package
/// fields (`registryBaseUrl`, `fileSha256`, runtime args, env vars) are
/// not surfaced because GR doesn't surface them either.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Package {
    /// Package registry type (`oci` / `npm` / `pypi` / `nuget` / `mcpb`).
    #[serde(rename = "registryType")]
    pub registry_type: String,
    /// Package identifier (name for npm/pypi/nuget, image ref for oci,
    /// download URL for mcpb).
    pub identifier: String,
    /// Package version. **Set to `""` when `registry_type == "oci"`** —
    /// pinned by GR `mcp.go:132-135` (`if pkg.RegistryType == "oci"
    /// { version = "" }`). Always emitted, even when empty (the registry
    /// omits version for OCI packages — handled in the builder, not via
    /// serde).
    #[serde(default)]
    pub version: String,
    /// Transport descriptor (always emitted).
    pub transport: Transport,
}

/// `model.Transport` mirror. Anodizer only surfaces `Type` (matching the
/// GR `mcp.go::Publish` loop, which sets `Transport: model.Transport{
/// Type: pkg.Transport.Type }` and nothing else).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Transport {
    /// Transport protocol (`stdio`, `streamable-http`, `sse`).
    #[serde(rename = "type")]
    pub kind: String,
}

/// `apiv0.ServerResponse` mirror — only the `_meta.io.modelcontextprotocol.registry/official.status`
/// field is consumed; the rest of the response is permissive.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerResponse {
    /// Registry-managed metadata. Optional because some mocks (and edge
    /// cases) emit empty `{}`.
    #[serde(rename = "_meta", default)]
    pub meta: ResponseMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResponseMeta {
    /// Official registry extensions block — JSON key uses the reverse-DNS
    /// namespacing convention from the upstream API.
    #[serde(
        rename = "io.modelcontextprotocol.registry/official",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub official: Option<RegistryExtensions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RegistryExtensions {
    /// Server lifecycle status (`active`, `deprecated`, `deleted`, or
    /// any of the publish-time intermediate values like `pending`,
    /// `approved`).
    #[serde(default)]
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_json_round_trips_with_npm_package() {
        // Mirrors the GR `TestPublishSuccess` expected payload — npm package,
        // populated repository, version === ctx.Version.
        let server = ServerJson {
            schema: CURRENT_SCHEMA_URL.to_string(),
            name: "test-server".to_string(),
            description: "A test MCP server".to_string(),
            title: "Test Server".to_string(),
            repository: Some(Repository {
                url: "https://github.com/test/repo".to_string(),
                source: "github".to_string(),
                id: "test/repo".to_string(),
                subfolder: String::new(),
            }),
            version: "1.0.0".to_string(),
            website_url: "https://example.com".to_string(),
            packages: vec![Package {
                registry_type: "npm".to_string(),
                identifier: "@test/server".to_string(),
                version: "1.0.0".to_string(),
                transport: Transport {
                    kind: "stdio".to_string(),
                },
            }],
        };
        let v = serde_json::to_value(&server).expect("serialize");

        // Pin the key shape — subfolder omitted, websiteUrl camelCased,
        // $schema present, registryType camelCased.
        assert_eq!(v["$schema"], CURRENT_SCHEMA_URL);
        assert_eq!(v["name"], "test-server");
        assert_eq!(v["websiteUrl"], "https://example.com");
        assert_eq!(v["repository"]["url"], "https://github.com/test/repo");
        assert!(v["repository"].get("subfolder").is_none());
        assert_eq!(v["packages"][0]["registryType"], "npm");
        assert_eq!(v["packages"][0]["version"], "1.0.0");
        assert_eq!(v["packages"][0]["transport"]["type"], "stdio");
    }

    #[test]
    fn server_json_omits_repository_when_url_empty() {
        // Per GR mcp.go:108-116, the entire repository object is omitted
        // when `mcp.Repository.URL == ""`. The publisher passes
        // `repository: None` in that branch; the JSON must reflect that.
        let server = ServerJson {
            schema: CURRENT_SCHEMA_URL.to_string(),
            name: "x/y".to_string(),
            description: String::new(),
            title: String::new(),
            repository: None,
            version: "1.0.0".to_string(),
            website_url: String::new(),
            packages: vec![],
        };
        let v = serde_json::to_value(&server).expect("serialize");
        assert!(v.get("repository").is_none(), "repository must be omitted");
        assert!(v.get("packages").is_none(), "empty packages omitted");
        assert!(v.get("title").is_none(), "empty title omitted");
        assert!(v.get("websiteUrl").is_none(), "empty websiteUrl omitted");
    }

    #[test]
    fn package_version_serializes_even_when_empty_for_oci() {
        // GR sets version="" for OCI but still emits it in the JSON
        // (no `omitempty` on the Go side for the version field in this
        // context). Anodizer mirrors that — the version key is always
        // present in a Package object.
        let pkg = Package {
            registry_type: "oci".to_string(),
            identifier: "ghcr.io/test/server:v1".to_string(),
            version: String::new(),
            transport: Transport {
                kind: "stdio".to_string(),
            },
        };
        let v = serde_json::to_value(&pkg).expect("serialize");
        assert_eq!(v["registryType"], "oci");
        assert!(v.get("version").is_some(), "version key must be present");
        assert_eq!(v["version"], "");
    }

    #[test]
    fn server_response_parses_pending_status() {
        let body = r#"{
            "_meta": {
                "io.modelcontextprotocol.registry/official": {
                    "status": "pending"
                }
            }
        }"#;
        let r: ServerResponse = serde_json::from_str(body).expect("parse");
        assert_eq!(r.meta.official.unwrap().status, "pending");
    }

    #[test]
    fn server_response_tolerates_missing_meta() {
        // Some test servers (and possibly real registries) emit `{}` or
        // omit `_meta` entirely. Don't fail parsing.
        let r: ServerResponse = serde_json::from_str("{}").expect("parse");
        assert!(r.meta.official.is_none());
    }
}
