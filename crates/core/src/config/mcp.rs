use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// MCP (Model Context Protocol) registry publisher config
// ---------------------------------------------------------------------------
//
// Mirrors GoReleaser's `MCP` / `MCPDetails` / `MCPRepository` / `MCPAuth` /
// `MCPPackage` / `MCPTransport` structs (`pkg/config/config.go:1561-1603`).
//
// Anodizer collapses GR's deprecated nested `mcp.github` migration shim — that
// alias only existed for backwards compatibility with early GR previews and
// has no consumers in this repo. The top-level fields are the canonical
// surface from day one.

/// MCP server registry publisher configuration.
///
/// Publishes an `apiv0.ServerJSON` document to the MCP registry
/// (`https://registry.modelcontextprotocol.io/v0/publish` by default).
/// Mirrors GoReleaser `config.MCP` + `config.MCPDetails` flattened.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Server name in reverse-DNS format (e.g. `io.github.user/weather`).
    /// Must contain exactly one forward slash separating namespace from
    /// server name. An empty / unset value skips the publisher entirely.
    pub name: Option<String>,

    /// Optional human-readable title shown in registry UIs (max 100 chars).
    /// Templated; supports `{{ .ProjectName | title }}`, `{{ .Version }}`, etc.
    pub title: Option<String>,

    /// Clear human-readable description of server functionality (max 100 chars).
    pub description: Option<String>,

    /// Optional URL to the server's homepage, documentation, or project
    /// website. Serialized as `websiteUrl` in the registry payload.
    pub homepage: Option<String>,

    /// Distribution packages — one entry per package registry (npm, pypi,
    /// nuget, oci, mcpb).
    pub packages: Vec<McpPackage>,

    /// Top-level transports list. Parsed for GoReleaser config-portability;
    /// the current MCP server schema derives transports per-package via the
    /// `packages[].transport` field, so this list is not currently emitted
    /// to the registry.
    pub transports: Vec<McpTransport>,

    /// Skip this publisher when the expression evaluates truthy. Accepts a
    /// bool or a Tera template that renders to `"true"`/`"false"` (e.g.
    /// `"{{ if .IsSnapshot }}true{{ endif }}"`). Accepts the legacy
    /// `disable:` spelling via serde alias for back-compat with imported
    /// GoReleaser configs (GR's MCP config field is `pkg/config/config.go`
    /// `MCP.Disable string`).
    #[serde(
        default,
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,

    /// Optional source repository metadata. Emitted as the `repository`
    /// object in the registry payload — omitted entirely when `url` is empty.
    pub repository: McpRepository,

    /// Authentication method for the registry's `/v0/publish` endpoint.
    /// Defaults to `none` (anonymous publish, allowed for development /
    /// staging registries).
    pub auth: McpAuth,

    /// Override the registry endpoint (for staging or a private mirror).
    /// Defaults to `https://registry.modelcontextprotocol.io` when unset.
    pub registry: Option<String>,
}

/// Repository metadata for the MCP registry payload.
/// Mirrors GoReleaser `config.MCPRepository` + upstream `model.Repository`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpRepository {
    /// Repository URL for browsing source code. Must support both web
    /// browsing and git-clone operations. An empty value omits the entire
    /// `repository` object from the published payload.
    pub url: String,

    /// Repository hosting service identifier. Used by registries to
    /// determine validation and API access methods.
    pub source: String,

    /// Repository identifier from the hosting service (e.g. GitHub repo ID).
    pub id: String,

    /// Optional relative path from repository root to the server location
    /// within a monorepo or nested package structure.
    pub subfolder: String,
}

/// Authentication method + token for the MCP registry's `/v0/publish`
/// endpoint. Mirrors GoReleaser `config.MCPAuth`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpAuth {
    /// Auth provider: `none` (anonymous), `github` (PAT exchange via
    /// `/v0/auth/github-at`), or `github-oidc` (Actions OIDC token exchange
    /// via `/v0/auth/github-oidc`). Templated.
    #[serde(rename = "type", default)]
    pub method: McpAuthMethod,

    /// Static token for the `none` and `github` methods. Templated, so
    /// `{{ envOrDefault "MCP_GITHUB_TOKEN" "" }}` works. Unused for
    /// `github-oidc` (the OIDC token is fetched from GitHub Actions at
    /// publish time).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

/// MCP auth method. Default is `None` (anonymous) which matches GoReleaser's
/// `mcp.go::Default` migration code (`cmp.Or(..., proto.MethodNone)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub enum McpAuthMethod {
    /// Anonymous publish — for testing or registries that allow it.
    /// Serializes / deserializes as `none`.
    #[default]
    #[serde(rename = "none")]
    None,
    /// GitHub Personal Access Token exchange via `/v0/auth/github-at`.
    /// Serializes / deserializes as `github`.
    #[serde(rename = "github")]
    Github,
    /// GitHub Actions OIDC token exchange via `/v0/auth/github-oidc`.
    /// Serializes / deserializes as `github-oidc`.
    #[serde(rename = "github-oidc")]
    GithubOidc,
}

impl McpAuthMethod {
    /// Parse the auth method from its over-the-wire string form. Accepts the
    /// three valid methods plus empty (treated as `None`, matching
    /// GoReleaser's `mcp.go::Default` defaulting behaviour).
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim() {
            "" | "none" => Ok(Self::None),
            "github" => Ok(Self::Github),
            "github-oidc" => Ok(Self::GithubOidc),
            other => anyhow::bail!(
                "mcp: unknown auth method '{}' (expected one of: none, github, github-oidc)",
                other
            ),
        }
    }

    /// Wire-format string for serialization + log output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Github => "github",
            Self::GithubOidc => "github-oidc",
        }
    }
}

/// A single package distribution descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpPackage {
    /// Registry type indicating how to download packages
    /// (e.g. `oci`, `npm`, `pypi`, `nuget`, `mcpb`).
    pub registry_type: McpRegistryType,

    /// Package identifier. For npm/pypi/nuget: the package name; for OCI:
    /// the full image reference (e.g. `ghcr.io/owner/repo:v1.0.0`); for
    /// mcpb: the download URL. Templated.
    pub identifier: String,

    /// Transport protocol configuration for this package.
    pub transport: McpTransport,
}

/// Package registry type — mirrors GoReleaser's `MCPPackage.RegistryType`
/// enum and upstream `model.RegistryType*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub enum McpRegistryType {
    /// OCI image (registry_type = "oci"). The `version` field in the
    /// published ServerJSON is intentionally empty for OCI packages — the
    /// version is encoded in the image identifier's `:tag` suffix.
    #[serde(rename = "oci")]
    Oci,
    /// npm registry (registry_type = "npm").
    #[default]
    #[serde(rename = "npm")]
    Npm,
    /// PyPI registry (registry_type = "pypi").
    #[serde(rename = "pypi")]
    Pypi,
    /// NuGet registry (registry_type = "nuget").
    #[serde(rename = "nuget")]
    Nuget,
    /// MCPB direct-download (registry_type = "mcpb").
    #[serde(rename = "mcpb")]
    Mcpb,
}

impl McpRegistryType {
    /// Wire-format string for serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Oci => "oci",
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Nuget => "nuget",
            Self::Mcpb => "mcpb",
        }
    }
}

/// Transport descriptor — mirrors GoReleaser's `MCPTransport` and
/// upstream `model.Transport`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpTransport {
    /// Transport type: `stdio`, `streamable-http`, or `sse`.
    #[serde(rename = "type", default)]
    pub kind: McpTransportType,
}

/// Transport protocol — mirrors upstream `model.TransportType*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub enum McpTransportType {
    /// Local stdio transport.
    #[default]
    #[serde(rename = "stdio")]
    Stdio,
    /// Streamable HTTP remote transport.
    #[serde(rename = "streamable-http")]
    StreamableHttp,
    /// Server-Sent Events remote transport.
    #[serde(rename = "sse")]
    Sse,
}

impl McpTransportType {
    /// Wire-format string for serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable-http",
            Self::Sse => "sse",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_method_default_is_none() {
        assert_eq!(McpAuthMethod::default(), McpAuthMethod::None);
        let auth = McpAuth::default();
        assert_eq!(auth.method, McpAuthMethod::None);
    }

    #[test]
    fn auth_method_parse_accepts_empty_as_none() {
        assert_eq!(McpAuthMethod::parse("").unwrap(), McpAuthMethod::None);
        assert_eq!(McpAuthMethod::parse("none").unwrap(), McpAuthMethod::None);
        assert_eq!(
            McpAuthMethod::parse("github").unwrap(),
            McpAuthMethod::Github
        );
        assert_eq!(
            McpAuthMethod::parse("github-oidc").unwrap(),
            McpAuthMethod::GithubOidc
        );
    }

    #[test]
    fn auth_method_parse_rejects_unknown() {
        let err = McpAuthMethod::parse("oauth").unwrap_err();
        assert!(err.to_string().contains("unknown auth method"));
    }

    #[test]
    fn yaml_roundtrip_minimal() {
        let yaml = r#"
name: io.github.test/server
title: Test
description: A test server
packages:
  - registry_type: oci
    identifier: ghcr.io/test/server:v1.0.0
    transport:
      type: stdio
auth:
  type: github-oidc
"#;
        let cfg: McpConfig = serde_yaml_ng::from_str(yaml).expect("parse mcp yaml");
        assert_eq!(cfg.name.as_deref(), Some("io.github.test/server"));
        assert_eq!(cfg.packages.len(), 1);
        assert_eq!(cfg.packages[0].registry_type, McpRegistryType::Oci);
        assert_eq!(cfg.packages[0].transport.kind, McpTransportType::Stdio);
        assert_eq!(cfg.auth.method, McpAuthMethod::GithubOidc);
    }

    #[test]
    fn yaml_roundtrip_skip_template() {
        let yaml = r#"
name: io.github.test/server
title: Test
description: A test server
skip: "{{ if .IsSnapshot }}true{{ endif }}"
"#;
        let cfg: McpConfig = serde_yaml_ng::from_str(yaml).expect("parse mcp yaml");
        assert!(cfg.skip.is_some());
        let s = cfg.skip.as_ref().unwrap();
        match s {
            StringOrBool::String(v) => assert!(v.contains("IsSnapshot")),
            _ => panic!("expected String variant"),
        }
    }

    #[test]
    fn yaml_roundtrip_disable_alias_for_back_compat() {
        // Legacy GR-imported configs use `disable:`; the alias should keep
        // parsing them as the canonical `skip:` field.
        let yaml = r#"
name: io.github.test/server
disable: "{{ if .IsSnapshot }}true{{ endif }}"
"#;
        let cfg: McpConfig = serde_yaml_ng::from_str(yaml).expect("parse mcp yaml");
        assert!(cfg.skip.is_some(), "disable: alias must populate skip");
    }

    #[test]
    fn auth_token_optional_and_omitted_when_empty() {
        // Tokens default to empty and stay out of the serialized form.
        let auth = McpAuth::default();
        let s = serde_yaml_ng::to_string(&auth).expect("serialize");
        assert!(s.contains("type: none"), "type field always rendered");
        assert!(!s.contains("token:"), "empty token omitted from yaml");
    }
}
