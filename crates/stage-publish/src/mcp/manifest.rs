//! Serde-shaped mirror of the MCP registry's `ServerJSON` / `Package` /
//! `Repository` / `Transport` wire schema.
//!
//! The wire format is JSON; field renames preserve the registry's JSON keys
//! (camelCase + `$schema`). `skip_serializing_if = "Option::is_none"` and
//! `Vec::is_empty` mirror the schema's `omitempty` fields so a minimal
//! config round-trips to the same payload the registry expects. The current
//! schema URL is
//! `https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json`.

use serde::{Deserialize, Serialize};

/// Current MCP server.json schema URL. The schema version string MUST be
/// kept in sync with the upstream registry when it bumps; bumping here is a
/// payload behaviour change so it gets a separate commit.
pub const CURRENT_SCHEMA_URL: &str =
    "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";

/// Default MCP registry base URL.
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.modelcontextprotocol.io";

/// The server JSON document POSTed to `/v0/publish`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerJson {
    /// JSON Schema URI for this server.json format (required).
    #[serde(rename = "$schema")]
    pub schema: String,
    /// Server name in reverse-DNS format (required).
    pub name: String,
    /// Clear human-readable description (required by upstream schema; we
    /// allow empty since the registry does not enforce non-empty here either).
    pub description: String,
    /// Optional human-readable title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Optional source-repository metadata. Omitted when the repository
    /// URL is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    /// Version string for this server. Required (≥1 char). Anodizer sets
    /// this from `ctx.version()` at publish time.
    pub version: String,
    /// Optional homepage / documentation URL. JSON key is `websiteUrl`
    /// (camelCase, single 'L').
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

/// Source-repository metadata sub-document.
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

/// Distribution-package sub-document. Anodizer only fills the small subset of fields
/// the registry populates — `RegistryType`, `Identifier`, `Version`,
/// `Transport` — set in the publish loop body. Other Package
/// fields (`registryBaseUrl`, `fileSha256`, runtime args, env vars) are
/// not surfaced because the registry does not consume them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Package {
    /// Package registry type (`oci` / `npm` / `pypi` / `nuget` / `mcpb`).
    #[serde(rename = "registryType")]
    pub registry_type: String,
    /// Package identifier (name for npm/pypi/nuget, image ref for oci,
    /// download URL for mcpb).
    pub identifier: String,
    /// Package version. **Set to `""` when `registry_type == "oci"`** —
    /// the OCI image reference already pins the version (e.g.
    /// `ghcr.io/foo/bar:v1.2.3`), so a separate `version` field is
    /// redundant. The MCP registry's OCI validator *rejects* a present
    /// version field ("OCI packages must not have 'version' field"),
    /// while its npm/pypi/nuget validators *require* a non-empty version.
    /// `skip_serializing_if = "String::is_empty"` reconciles both: an
    /// empty string is dropped from the wire (satisfying the OCI rule),
    /// and non-OCI callers always supply a concrete version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// Transport descriptor (always emitted).
    pub transport: Transport,
}

/// Package transport. Carries `type` for every transport plus the remote-only
/// `url` + `headers` the registry requires for `streamable-http` / `sse`. A
/// `stdio` transport leaves `url` empty and `headers` empty, so both keys are
/// dropped from the wire — the registry's `stdio` subschema forbids a `url`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Transport {
    /// Transport protocol (`stdio`, `streamable-http`, `sse`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Endpoint URL for remote transports. Omitted when empty so `stdio`
    /// transports never emit a `url` key (the schema forbids it there).
    #[serde(rename = "url", default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// HTTP headers for remote transports. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
}

/// A single transport HTTP header — the registry's `KeyValueInput`
/// (`{ name, value }`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Header {
    /// Header name.
    pub name: String,
    /// Header value. Omitted when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
}

/// Server response — only the `_meta.io.modelcontextprotocol.registry/official.status`
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
        // Expected payload — npm package,
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
                    ..Transport::default()
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
        // The entire repository object is omitted
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
    fn package_version_omitted_when_empty_for_oci() {
        // The MCP registry's OCI validator rejects a present `version`
        // field, so an empty version for an OCI package must be dropped
        // from the wire entirely rather than serialized as `""`. The
        // `skip_serializing_if = "String::is_empty"` attribute is what
        // makes that omission happen.
        let pkg = Package {
            registry_type: "oci".to_string(),
            identifier: "ghcr.io/test/server:v1".to_string(),
            version: String::new(),
            transport: Transport {
                kind: "stdio".to_string(),
                ..Transport::default()
            },
        };
        let v = serde_json::to_value(&pkg).expect("serialize");
        assert_eq!(v["registryType"], "oci");
        assert!(
            v.get("version").is_none(),
            "version key must be omitted when empty (the registry rejects an empty version)"
        );
    }

    #[test]
    fn package_version_present_when_non_empty() {
        // Non-OCI registry types (npm/pypi/nuget/mcpb) require the
        // version field — confirm serde still emits it when populated.
        let pkg = Package {
            registry_type: "npm".to_string(),
            identifier: "@test/server".to_string(),
            version: "1.2.3".to_string(),
            transport: Transport {
                kind: "stdio".to_string(),
                ..Transport::default()
            },
        };
        let v = serde_json::to_value(&pkg).expect("serialize");
        assert_eq!(v["registryType"], "npm");
        assert_eq!(v["version"], "1.2.3");
    }

    #[test]
    fn stdio_transport_omits_url_and_headers() {
        // The registry's stdio subschema forbids a `url` and has no headers,
        // so an empty url/headers must drop both keys from the wire.
        let t = Transport {
            kind: "stdio".to_string(),
            url: String::new(),
            headers: vec![],
        };
        let v = serde_json::to_value(&t).expect("serialize");
        assert_eq!(v["type"], "stdio");
        assert!(v.get("url").is_none(), "stdio must not emit a url key");
        assert!(v.get("headers").is_none(), "stdio must not emit headers");
    }

    #[test]
    fn remote_transport_emits_url_and_headers() {
        // A streamable-http transport carries the required url plus an array
        // of `{name, value}` headers matching the schema's KeyValueInput.
        let t = Transport {
            kind: "streamable-http".to_string(),
            url: "https://api.example.com/mcp".to_string(),
            headers: vec![Header {
                name: "Authorization".to_string(),
                value: "Bearer abc123".to_string(),
            }],
        };
        let v = serde_json::to_value(&t).expect("serialize");
        assert_eq!(v["type"], "streamable-http");
        assert_eq!(v["url"], "https://api.example.com/mcp");
        assert_eq!(v["headers"][0]["name"], "Authorization");
        assert_eq!(v["headers"][0]["value"], "Bearer abc123");
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
