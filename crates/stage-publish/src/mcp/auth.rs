//! MCP registry authentication providers.
//!
//! Each provider matches a `McpAuthMethod` variant and produces a bearer
//! token suitable for `POST {registry}/v0/publish`. The Rust implementations
//! mirror the upstream Go sources:
//!
//! - `auth/none.go`        — anonymous: POST `/v0/auth/none` -> registry JWT
//! - `auth/github-at.go`   — PAT exchange: POST `/v0/auth/github-at` -> JWT
//! - `auth/github-oidc.go` — Actions OIDC: GET `${ACTIONS_ID_TOKEN_REQUEST_URL}
//!                            &audience=<registry>` then POST `/v0/auth/github-oidc`
//!
//! Anodizer keeps tokens in memory only — GR writes
//! `.mcpregistry_github_token` / `.mcpregistry_registry_token` to the cwd
//! and deletes them post-publish; we skip those on-disk files entirely.
//! Network shape (URLs, body, headers) is preserved end-to-end so the
//! registry can't tell the difference.
//!
//! The retry policy is threaded through every HTTP call — auth-exchange
//! 5xx / transport failures retry per the user's top-level `retry:` block,
//! 4xx fast-fails so a bad token surfaces immediately instead of after
//! a 10-attempt sleep cascade.

use std::time::Duration;

use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anodizer_core::url::percent_encode_unreserved;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use anodizer_core::config::McpAuthMethod;

/// Anything that can produce a bearer token for `Authorization: Bearer ...`.
///
/// Mirrors the upstream `auth.Provider` interface — minus the `Name()`
/// accessor (which only existed for log lines in the GR CLI; we log the
/// method via `McpAuthMethod::as_str()` directly).
pub trait McpAuthProvider {
    /// Perform any pre-token work (currently a no-op for all in-tree
    /// providers — GR's interactive device-flow login is intentionally
    /// not surfaced because anodizer is non-interactive). Errors abort
    /// the publish with a `could not login: ...` message matching GR.
    fn login(&self) -> Result<()> {
        Ok(())
    }

    /// Produce the bearer token used in `Authorization: Bearer <token>`.
    /// Implementations may perform a token-exchange HTTP call here.
    fn get_token(&self) -> Result<String>;
}

// ---------------------------------------------------------------------------
// Common request/response shapes
// ---------------------------------------------------------------------------

/// `auth.RegistryTokenResponse` mirror — the body returned by the registry's
/// token-exchange endpoints (`/v0/auth/none`, `/v0/auth/github-at`,
/// `/v0/auth/github-oidc`). `expires_at` is currently unused on our side.
#[derive(Debug, Deserialize)]
struct RegistryTokenResponse {
    /// Registry JWT to use as the `Bearer` token for `/v0/publish`.
    #[serde(default)]
    registry_token: String,
}

/// Body shape for `/v0/auth/github-at` (PAT exchange). Mirrors upstream
/// `github-at.go:300`'s `map[string]string{"github_token": ...}`.
#[derive(Debug, Serialize)]
struct GithubAtBody<'a> {
    github_token: &'a str,
}

/// Body shape for `/v0/auth/github-oidc` (OIDC exchange). Mirrors upstream
/// `github-oidc.go:66`'s `map[string]string{"oidc_token": ...}`.
#[derive(Debug, Serialize)]
struct GithubOidcBody<'a> {
    oidc_token: &'a str,
}

/// `GitHub Actions ID-token` response wrapping a `value` field. Mirrors
/// upstream `github-oidc.go:153-156`.
#[derive(Debug, Deserialize)]
struct OidcTokenValue {
    #[serde(default)]
    value: String,
}

// ---------------------------------------------------------------------------
// Provider factory
// ---------------------------------------------------------------------------

/// Build the auth provider for a given method. Mirrors GR `mcp.go::authProvider`.
///
/// `registry_url` is the base URL (no trailing slash) — the providers append
/// `/v0/auth/...` paths themselves. `token` is the static token, only
/// consumed by the `None` (anonymous override) and `Github` (PAT) variants;
/// `GithubOidc` ignores it entirely (the id-token is fetched from the
/// Actions runtime).
pub fn provider_for(
    method: McpAuthMethod,
    registry_url: &str,
    token: &str,
    policy: &RetryPolicy,
) -> Box<dyn McpAuthProvider> {
    match method {
        McpAuthMethod::None => Box::new(NoneAuthProvider {
            registry_url: registry_url.to_string(),
            token: token.to_string(),
            policy: *policy,
        }),
        McpAuthMethod::Github => Box::new(GithubAtAuthProvider {
            registry_url: registry_url.to_string(),
            token: token.to_string(),
            policy: *policy,
        }),
        McpAuthMethod::GithubOidc => Box::new(GithubOidcAuthProvider {
            registry_url: registry_url.to_string(),
            policy: *policy,
        }),
    }
}

// ---------------------------------------------------------------------------
// NoneAuthProvider — anonymous (or static-token override)
// ---------------------------------------------------------------------------

/// `auth.NoneProvider` mirror. Two behaviours:
///
/// - When `token` is non-empty, return it verbatim. This is an anodizer
///   extension — useful for staging registries that accept a pre-issued JWT
///   without going through `/v0/auth/none`. The upstream Go provider does
///   honour an in-memory token field on the same code path
///   (`none.go:28-32`), so the wire behaviour is equivalent.
/// - When `token` is empty, POST `/v0/auth/none` and return the
///   `registry_token` from the response. Mirrors upstream `none.go:33-62`.
pub struct NoneAuthProvider {
    pub registry_url: String,
    pub token: String,
    pub policy: RetryPolicy,
}

impl McpAuthProvider for NoneAuthProvider {
    fn get_token(&self) -> Result<String> {
        if !self.token.is_empty() {
            return Ok(self.token.clone());
        }
        let url = format!("{}/v0/auth/none", self.registry_url.trim_end_matches('/'));
        let client = build_client(Duration::from_secs(30))?;
        let (_, body) = retry_http_blocking(
            "mcp: /v0/auth/none",
            &self.policy,
            SuccessClass::Strict,
            |_| client.post(&url).send(),
            |status, body| {
                format!(
                    "mcp: POST {} returned HTTP {}: {}",
                    url,
                    status,
                    anodizer_core::redact::redact_bearer_tokens(body)
                )
            },
        )
        .context("mcp: anonymous token exchange")?;
        let parsed: RegistryTokenResponse =
            serde_json::from_str(&body).context("mcp: parse anonymous token response")?;
        if parsed.registry_token.is_empty() {
            anyhow::bail!("mcp: anonymous token response missing registry_token");
        }
        Ok(parsed.registry_token)
    }
}

// ---------------------------------------------------------------------------
// GithubAtAuthProvider — PAT exchange
// ---------------------------------------------------------------------------

/// `auth.GitHubATProvider` mirror — the non-interactive branch.
///
/// GR's `github-at.go::Login` supports both an explicit `--token` (or
/// `MCP_GITHUB_TOKEN` env var) AND an interactive device-code flow. Anodizer
/// is non-interactive by design, so we only support the explicit token. If
/// `token` is empty we fall back to the `MCP_GITHUB_TOKEN` env var to keep
/// the env-var ergonomics GR users expect; both empty is a hard error.
pub struct GithubAtAuthProvider {
    pub registry_url: String,
    pub token: String,
    pub policy: RetryPolicy,
}

impl McpAuthProvider for GithubAtAuthProvider {
    fn get_token(&self) -> Result<String> {
        let github_token = if self.token.is_empty() {
            std::env::var("MCP_GITHUB_TOKEN").unwrap_or_default()
        } else {
            self.token.clone()
        };
        if github_token.is_empty() {
            anyhow::bail!(
                "mcp: auth.type=github requires either auth.token in config \
                 or the MCP_GITHUB_TOKEN environment variable; both were empty"
            );
        }
        let url = format!(
            "{}/v0/auth/github-at",
            self.registry_url.trim_end_matches('/')
        );
        let body_json = serde_json::to_string(&GithubAtBody {
            github_token: &github_token,
        })
        .context("mcp: serialize github-at request body")?;
        let client = build_client(Duration::from_secs(30))?;
        let (_, response_body) = retry_http_blocking(
            "mcp: /v0/auth/github-at",
            &self.policy,
            SuccessClass::Strict,
            |_| {
                client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .body(body_json.clone())
                    .send()
            },
            |status, body| {
                format!(
                    "mcp: POST {} returned HTTP {}: {}",
                    url,
                    status,
                    anodizer_core::redact::redact_bearer_tokens(body)
                )
            },
        )
        .context("mcp: github PAT exchange")?;
        let parsed: RegistryTokenResponse =
            serde_json::from_str(&response_body).context("mcp: parse github-at token response")?;
        if parsed.registry_token.is_empty() {
            anyhow::bail!("mcp: github-at response missing registry_token");
        }
        Ok(parsed.registry_token)
    }
}

// ---------------------------------------------------------------------------
// GithubOidcAuthProvider — Actions id-token exchange
// ---------------------------------------------------------------------------

/// `auth.GitHubOIDCProvider` mirror. Two-step:
///
/// 1. GET `${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=<registry-audience>` with
///    `Authorization: Bearer ${ACTIONS_ID_TOKEN_REQUEST_TOKEN}` -> `{"value":"..."}`.
/// 2. POST `{registry}/v0/auth/github-oidc` with `{"oidc_token":"<value>"}` ->
///    `{"registry_token":"..."}`.
///
/// The audience is `scheme://lowercase-host` of the registry URL — mirrors
/// upstream `github-oidc.go::audienceFromRegistryURL`.
pub struct GithubOidcAuthProvider {
    pub registry_url: String,
    pub policy: RetryPolicy,
}

impl McpAuthProvider for GithubOidcAuthProvider {
    fn get_token(&self) -> Result<String> {
        let request_url = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").map_err(|_| {
            anyhow::anyhow!(
                "mcp: auth.type=github-oidc requires ACTIONS_ID_TOKEN_REQUEST_URL \
                 (set automatically by GitHub Actions runners with id-token: write \
                 permission)"
            )
        })?;
        let request_token = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").map_err(|_| {
            anyhow::anyhow!(
                "mcp: auth.type=github-oidc requires ACTIONS_ID_TOKEN_REQUEST_TOKEN \
                 (set automatically by GitHub Actions runners with id-token: write \
                 permission)"
            )
        })?;
        if request_url.is_empty() || request_token.is_empty() {
            anyhow::bail!(
                "mcp: auth.type=github-oidc: ACTIONS_ID_TOKEN_REQUEST_URL/TOKEN \
                 are empty — id-token: write permission missing from workflow"
            );
        }

        let audience = audience_from_registry_url(&self.registry_url)?;
        let separator = if request_url.contains('?') { '&' } else { '?' };
        let full_url = format!(
            "{}{}audience={}",
            request_url,
            separator,
            percent_encode_unreserved(&audience)
        );

        let client = build_client(Duration::from_secs(30))?;
        let (_, oidc_body) = retry_http_blocking(
            "mcp: GitHub Actions OIDC token",
            &self.policy,
            SuccessClass::Strict,
            |_| {
                client
                    .get(&full_url)
                    .header("Authorization", format!("Bearer {}", request_token))
                    .header("Accept", "application/json")
                    .send()
            },
            |status, body| {
                format!(
                    "mcp: GET {} returned HTTP {}: {}",
                    full_url,
                    status,
                    anodizer_core::redact::redact_bearer_tokens(body)
                )
            },
        )
        .context("mcp: fetch GitHub Actions id-token")?;
        let oidc: OidcTokenValue =
            serde_json::from_str(&oidc_body).context("mcp: parse OIDC token response")?;
        if oidc.value.is_empty() {
            anyhow::bail!("mcp: OIDC token response missing value");
        }

        let exchange_url = format!(
            "{}/v0/auth/github-oidc",
            self.registry_url.trim_end_matches('/')
        );
        let body_json = serde_json::to_string(&GithubOidcBody {
            oidc_token: &oidc.value,
        })
        .context("mcp: serialize github-oidc exchange body")?;
        let (_, exchange_body) = retry_http_blocking(
            "mcp: /v0/auth/github-oidc",
            &self.policy,
            SuccessClass::Strict,
            |_| {
                client
                    .post(&exchange_url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .body(body_json.clone())
                    .send()
            },
            |status, body| {
                format!(
                    "mcp: POST {} returned HTTP {}: {}",
                    exchange_url,
                    status,
                    anodizer_core::redact::redact_bearer_tokens(body)
                )
            },
        )
        .context("mcp: github-oidc exchange")?;
        let parsed: RegistryTokenResponse = serde_json::from_str(&exchange_body)
            .context("mcp: parse github-oidc registry-token response")?;
        if parsed.registry_token.is_empty() {
            anyhow::bail!("mcp: github-oidc response missing registry_token");
        }
        Ok(parsed.registry_token)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a blocking HTTP client with the shared anodizer user-agent +
/// the supplied timeout. Mirrors the dockerhub publisher's client config so
/// behaviour is consistent across HTTP-uploading publishers; the dynamic
/// user-agent picks up the crate version at compile time.
pub(crate) fn build_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "anodizer/",
            env!("CARGO_PKG_VERSION"),
            " (mcp-publisher)"
        ))
        .timeout(timeout)
        .build()
        .context("mcp: build HTTP client")
}

/// Compute the OIDC audience from a registry URL: `scheme://lowercase-host[:port]`.
///
/// Mirrors upstream Go `net/url`'s `u.Host` (host:port). The audience claim is
/// the upstream-identity used by the registry to verify the id-token; matching
/// upstream avoids token-rejection on private mirrors that bind to a non-default
/// port (e.g. `http://mcp.internal:8080`).
fn audience_from_registry_url(url: &str) -> Result<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        anyhow::bail!("mcp: registry URL is empty");
    }
    let parsed = reqwest::Url::parse(trimmed)
        .with_context(|| format!("mcp: parse registry URL {:?}", trimmed))?;
    let scheme = parsed.scheme();
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("mcp: registry URL missing host: {}", trimmed))?;
    if scheme.is_empty() {
        anyhow::bail!("mcp: registry URL must include a scheme: {}", trimmed);
    }
    let lower_host = host.to_ascii_lowercase();
    match parsed.port() {
        Some(port) => Ok(format!("{}://{}:{}", scheme, lower_host, port)),
        None => Ok(format!("{}://{}", scheme, lower_host)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_uses_scheme_and_lowercased_host() {
        let a = audience_from_registry_url("https://Registry.ModelContextProtocol.IO").unwrap();
        assert_eq!(a, "https://registry.modelcontextprotocol.io");
    }

    #[test]
    fn audience_includes_explicit_port() {
        // Mirrors upstream Go net/url u.Host (host:port). A private mirror at
        // a non-default port must produce an audience claim including the port;
        // anodizer previously stripped it (reqwest::Url::host_str() omits port),
        // diverging from upstream and risking token rejection.
        let a = audience_from_registry_url("http://mcp.internal:8080").unwrap();
        assert_eq!(a, "http://mcp.internal:8080");
        let a = audience_from_registry_url("https://Mirror.Example.COM:9443/v0").unwrap();
        assert_eq!(a, "https://mirror.example.com:9443");
    }

    #[test]
    fn audience_omits_default_port() {
        // reqwest::Url::port() returns None for default ports (80, 443) even
        // when present in the input — matches Go's url.Host behavior of only
        // emitting the port when it differs from the scheme default.
        let a = audience_from_registry_url("https://registry.modelcontextprotocol.io").unwrap();
        assert_eq!(a, "https://registry.modelcontextprotocol.io");
    }

    #[test]
    fn audience_rejects_empty_or_invalid() {
        assert!(audience_from_registry_url("").is_err());
        assert!(audience_from_registry_url("   ").is_err());
        assert!(audience_from_registry_url("not-a-url").is_err());
    }
}
