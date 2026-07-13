//! MCP registry authentication providers.
//!
//! Each provider matches a `McpAuthMethod` variant and produces a bearer
//! token suitable for `POST {registry}/v0/publish`:
//!
//! - anonymous: POST `/v0/auth/none` -> registry JWT
//! - PAT exchange: POST `/v0/auth/github-at` -> JWT
//! - Actions OIDC: GET `${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=<registry>`
//!   then POST `/v0/auth/github-oidc`
//!
//! Tokens are kept in memory only — no `.mcpregistry_*_token` files are
//! written to the cwd.
//!
//! The retry policy is threaded through every HTTP call — auth-exchange
//! 5xx / transport failures retry per the user's top-level `retry:` block,
//! 4xx fast-fails so a bad token surfaces immediately instead of after
//! a 10-attempt sleep cascade.

use std::sync::Arc;
use std::time::Duration;

use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking};
use anodizer_core::{EnvSource, ProcessEnvSource};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use anodizer_core::config::McpAuthMethod;

/// Anything that can produce a bearer token for `Authorization: Bearer ...`.
///
/// The auth method is logged via `McpAuthMethod::as_str()` directly.
pub trait McpAuthProvider {
    /// Perform any pre-token work (currently a no-op for all in-tree
    /// providers — interactive device-flow login is intentionally not
    /// surfaced because anodizer is non-interactive). Errors abort the
    /// publish with a `could not login: ...` message.
    fn login(&self) -> Result<()> {
        Ok(())
    }

    /// Produce the bearer token used in `Authorization: Bearer <token>`.
    /// Implementations may perform a token-exchange HTTP call here; `log`
    /// surfaces their per-attempt retry warns.
    fn get_token(&self, log: &anodizer_core::log::StageLogger) -> Result<String>;
}

// ---------------------------------------------------------------------------
// Common request/response shapes
// ---------------------------------------------------------------------------

/// The token-response body returned by the registry's
/// token-exchange endpoints (`/v0/auth/none`, `/v0/auth/github-at`,
/// `/v0/auth/github-oidc`). `expires_at` is currently unused on our side.
#[derive(Debug, Deserialize)]
struct RegistryTokenResponse {
    /// Registry JWT to use as the `Bearer` token for `/v0/publish`.
    #[serde(default)]
    registry_token: String,
}

/// Body shape for `/v0/auth/github-at` (PAT exchange): a JSON object
/// `{"github_token": ...}`.
#[derive(Debug, Serialize)]
struct GithubAtBody<'a> {
    github_token: &'a str,
}

/// Body shape for `/v0/auth/github-oidc` (OIDC exchange): a JSON object
/// `{"oidc_token": ...}`.
#[derive(Debug, Serialize)]
struct GithubOidcBody<'a> {
    oidc_token: &'a str,
}

// ---------------------------------------------------------------------------
// Provider factory
// ---------------------------------------------------------------------------

/// Build the auth provider for a given method.
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
    provider_for_with_env(
        method,
        registry_url,
        token,
        policy,
        Arc::new(ProcessEnvSource),
    )
}

/// Env-injectable form of [`provider_for`]. Production wires up
/// [`ProcessEnvSource`]; unit tests pass an
/// [`anodizer_core::MapEnvSource`] so the env-driven fallbacks
/// (`MCP_GITHUB_TOKEN`, `ACTIONS_ID_TOKEN_REQUEST_URL/TOKEN`) can
/// be exercised without mutating the process env.
pub fn provider_for_with_env(
    method: McpAuthMethod,
    registry_url: &str,
    token: &str,
    policy: &RetryPolicy,
    env: Arc<dyn EnvSource>,
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
            env: Arc::clone(&env),
        }),
        McpAuthMethod::GithubOidc => Box::new(GithubOidcAuthProvider {
            registry_url: registry_url.to_string(),
            policy: *policy,
            env: Arc::clone(&env),
        }),
    }
}

// ---------------------------------------------------------------------------
// NoneAuthProvider — anonymous (or static-token override)
// ---------------------------------------------------------------------------

/// Anonymous auth provider. Two behaviours:
///
/// - When `token` is non-empty, return it verbatim — useful for staging
///   registries that accept a pre-issued JWT without going through
///   `/v0/auth/none`.
/// - When `token` is empty, POST `/v0/auth/none` and return the
///   `registry_token` from the response.
pub struct NoneAuthProvider {
    pub registry_url: String,
    pub token: String,
    pub policy: RetryPolicy,
}

impl McpAuthProvider for NoneAuthProvider {
    fn get_token(&self, log: &anodizer_core::log::StageLogger) -> Result<String> {
        if !self.token.is_empty() {
            return Ok(self.token.clone());
        }
        let url = format!("{}/v0/auth/none", self.registry_url.trim_end_matches('/'));
        let client = build_client(Duration::from_secs(30))?;
        let (_, body) = retry_http_blocking(
            RetryLog::new("mcp: /v0/auth/none", log),
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

/// PAT-exchange auth provider — the non-interactive branch.
///
/// Anodizer is non-interactive by design, so only the explicit token is
/// supported (no device-code flow). If `token` is empty it falls back to
/// the `MCP_GITHUB_TOKEN` env var; both empty is a hard error.
pub struct GithubAtAuthProvider {
    pub registry_url: String,
    pub token: String,
    pub policy: RetryPolicy,
    /// Injected env source for resolving the `MCP_GITHUB_TOKEN`
    /// fallback. Production passes [`ProcessEnvSource`]; tests inject
    /// a [`anodizer_core::MapEnvSource`].
    pub env: Arc<dyn EnvSource>,
}

impl McpAuthProvider for GithubAtAuthProvider {
    fn get_token(&self, log: &anodizer_core::log::StageLogger) -> Result<String> {
        // Two resolution sources: config `auth.token` first, then the
        // `MCP_GITHUB_TOKEN` env var. `unwrap_or_default()` collapses
        // "var unset" and "var set to empty string" into the same empty-
        // string sentinel; the explicit `is_empty()` check below fails
        // fast with a single actionable error covering both states.
        let github_token = if self.token.is_empty() {
            self.env.var("MCP_GITHUB_TOKEN").unwrap_or_default()
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
            RetryLog::new("mcp: /v0/auth/github-at", log),
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

/// GitHub Actions OIDC auth provider. Two-step:
///
/// 1. GET `${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=<registry-audience>` with
///    `Authorization: Bearer ${ACTIONS_ID_TOKEN_REQUEST_TOKEN}` -> `{"value":"..."}`.
/// 2. POST `{registry}/v0/auth/github-oidc` with `{"oidc_token":"<value>"}` ->
///    `{"registry_token":"..."}`.
///
/// The audience is `scheme://lowercase-host` of the registry URL — mirrors
/// the registry URL used as the OIDC audience.
pub struct GithubOidcAuthProvider {
    pub registry_url: String,
    pub policy: RetryPolicy,
    /// Injected env source for the Actions OIDC token fetch
    /// (`ACTIONS_ID_TOKEN_REQUEST_URL` / `ACTIONS_ID_TOKEN_REQUEST_TOKEN`).
    /// Production passes [`ProcessEnvSource`]; tests inject a
    /// [`anodizer_core::MapEnvSource`].
    pub env: Arc<dyn EnvSource>,
}

impl McpAuthProvider for GithubOidcAuthProvider {
    fn get_token(&self, log: &anodizer_core::log::StageLogger) -> Result<String> {
        // Hop 1: fetch the Actions id-token for the registry-derived audience
        // (shared with every other OIDC publisher).
        let audience = audience_from_registry_url(&self.registry_url)?;
        let oidc_value = crate::actions_oidc::request_id_token(
            |k| self.env.var(k),
            &audience,
            &self.policy,
            log,
            "mcp",
        )?;

        // Hop 2: exchange the JWT at the MCP registry's github-oidc endpoint.
        let client = build_client(Duration::from_secs(30))?;
        let exchange_url = format!(
            "{}/v0/auth/github-oidc",
            self.registry_url.trim_end_matches('/')
        );
        let body_json = serde_json::to_string(&GithubOidcBody {
            oidc_token: &oidc_value,
        })
        .context("mcp: serialize github-oidc exchange body")?;
        let (_, exchange_body) = retry_http_blocking(
            RetryLog::new("mcp: /v0/auth/github-oidc", log),
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

    fn log() -> anodizer_core::log::StageLogger {
        anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet)
    }

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

    #[test]
    fn audience_rejects_url_without_host() {
        // `data:` URLs parse but carry no authority component, so there is
        // no host to derive an audience claim from.
        let err = audience_from_registry_url("data:text/plain,x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing host"), "{err}");
    }

    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use anodizer_core::MapEnvSource;
    use anodizer_core::config::McpAuthMethod;
    use anodizer_core::retry::RetryPolicy;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with;

    /// Tight policy so retry tests complete in milliseconds rather than the
    /// production 10-attempt / 10s-base cascade.
    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    fn http_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn empty_env() -> Arc<MapEnvSource> {
        Arc::new(MapEnvSource::new())
    }

    // -- NoneAuthProvider ---------------------------------------------------

    #[test]
    fn none_provider_returns_static_token_without_network() {
        let p = NoneAuthProvider {
            registry_url: "http://127.0.0.1:1".to_string(),
            token: "preissued-jwt".to_string(),
            policy: fast_policy(),
        };
        p.login().expect("default login is a no-op");
        assert_eq!(p.get_token(&log()).unwrap(), "preissued-jwt");
    }

    #[test]
    fn none_provider_exchanges_via_auth_none() {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![http_response(
                "200 OK",
                r#"{"registry_token":"anon-reg-jwt"}"#,
            )]
        });
        let p = NoneAuthProvider {
            // Trailing slash exercises the trim_end_matches('/') join.
            registry_url: format!("http://{addr}/"),
            token: String::new(),
            policy: fast_policy(),
        };
        assert_eq!(p.get_token(&log()).unwrap(), "anon-reg-jwt");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn none_provider_missing_registry_token_errors() {
        let (addr, _calls) =
            spawn_oneshot_http_responder_with(|_| vec![http_response("200 OK", "{}")]);
        let p = NoneAuthProvider {
            registry_url: format!("http://{addr}"),
            token: String::new(),
            policy: fast_policy(),
        };
        let err = p.get_token(&log()).unwrap_err().to_string();
        assert!(err.contains("missing registry_token"), "{err}");
    }

    #[test]
    fn none_provider_unparseable_body_errors() {
        let (addr, _calls) =
            spawn_oneshot_http_responder_with(|_| vec![http_response("200 OK", "not-json")]);
        let p = NoneAuthProvider {
            registry_url: format!("http://{addr}"),
            token: String::new(),
            policy: fast_policy(),
        };
        let err = format!("{:#}", p.get_token(&log()).unwrap_err());
        assert!(err.contains("parse anonymous token response"), "{err}");
    }

    #[test]
    fn none_provider_4xx_fast_fails_with_status_and_body() {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![http_response(
                "401 Unauthorized",
                r#"{"error":"no anonymous auth here"}"#,
            )]
        });
        let p = NoneAuthProvider {
            registry_url: format!("http://{addr}"),
            token: String::new(),
            policy: fast_policy(),
        };
        let err = format!("{:#}", p.get_token(&log()).unwrap_err());
        assert!(err.contains("HTTP 401"), "{err}");
        assert!(err.contains("no anonymous auth here"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx must not retry");
    }

    // -- GithubAtAuthProvider -----------------------------------------------

    #[test]
    fn github_at_exchanges_config_token() {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![http_response(
                "200 OK",
                r#"{"registry_token":"pat-reg-jwt"}"#,
            )]
        });
        let p = provider_for_with_env(
            McpAuthMethod::Github,
            &format!("http://{addr}/"),
            "ghp_config_token",
            &fast_policy(),
            empty_env(),
        );
        assert_eq!(p.get_token(&log()).unwrap(), "pat-reg-jwt");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn github_at_falls_back_to_env_token() {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![http_response(
                "200 OK",
                r#"{"registry_token":"env-reg-jwt"}"#,
            )]
        });
        let env = Arc::new(MapEnvSource::new().with("MCP_GITHUB_TOKEN", "ghp_env_token"));
        let p = provider_for_with_env(
            McpAuthMethod::Github,
            &format!("http://{addr}"),
            "",
            &fast_policy(),
            env,
        );
        assert_eq!(p.get_token(&log()).unwrap(), "env-reg-jwt");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn github_at_errors_when_no_token_anywhere() {
        let p = provider_for_with_env(
            McpAuthMethod::Github,
            "http://127.0.0.1:1",
            "",
            &fast_policy(),
            empty_env(),
        );
        let err = p.get_token(&log()).unwrap_err().to_string();
        assert!(err.contains("MCP_GITHUB_TOKEN"), "{err}");
        assert!(err.contains("auth.token"), "{err}");
    }

    #[test]
    fn github_at_missing_registry_token_errors() {
        let (addr, _calls) =
            spawn_oneshot_http_responder_with(|_| vec![http_response("200 OK", "{}")]);
        let p = provider_for_with_env(
            McpAuthMethod::Github,
            &format!("http://{addr}"),
            "ghp_x",
            &fast_policy(),
            empty_env(),
        );
        let err = p.get_token(&log()).unwrap_err().to_string();
        assert!(
            err.contains("github-at response missing registry_token"),
            "{err}"
        );
    }

    #[test]
    fn github_at_4xx_fast_fails_with_status() {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![http_response("403 Forbidden", r#"{"error":"bad PAT"}"#)]
        });
        let p = provider_for_with_env(
            McpAuthMethod::Github,
            &format!("http://{addr}"),
            "ghp_bad",
            &fast_policy(),
            empty_env(),
        );
        let err = format!("{:#}", p.get_token(&log()).unwrap_err());
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(err.contains("bad PAT"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx must not retry");
    }

    // -- GithubOidcAuthProvider ---------------------------------------------

    #[test]
    fn oidc_requires_request_url_env() {
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            "http://127.0.0.1:1",
            "",
            &fast_policy(),
            empty_env(),
        );
        let err = p.get_token(&log()).unwrap_err().to_string();
        assert!(err.contains("ACTIONS_ID_TOKEN_REQUEST_URL"), "{err}");
    }

    #[test]
    fn oidc_requires_request_token_env() {
        let env = Arc::new(
            MapEnvSource::new().with("ACTIONS_ID_TOKEN_REQUEST_URL", "http://127.0.0.1:1/id"),
        );
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            "http://127.0.0.1:1",
            "",
            &fast_policy(),
            env,
        );
        let err = p.get_token(&log()).unwrap_err().to_string();
        assert!(err.contains("ACTIONS_ID_TOKEN_REQUEST_TOKEN"), "{err}");
    }

    #[test]
    fn oidc_rejects_empty_env_values() {
        let env = Arc::new(
            MapEnvSource::new()
                .with("ACTIONS_ID_TOKEN_REQUEST_URL", "")
                .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", ""),
        );
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            "http://127.0.0.1:1",
            "",
            &fast_policy(),
            env,
        );
        let err = p.get_token(&log()).unwrap_err().to_string();
        // Empty and absent collapse to one message via the shared hop-1 helper:
        // it names the missing request var and the id-token: write cause.
        assert!(err.contains("id-token: write permission"), "{err}");
        assert!(err.contains("ACTIONS_ID_TOKEN_REQUEST_URL"), "{err}");
    }

    #[test]
    fn oidc_two_step_exchange_succeeds() {
        // One responder serves both steps in order: the Actions id-token GET,
        // then the registry POST /v0/auth/github-oidc.
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![
                http_response("200 OK", r#"{"value":"actions-oidc-jwt"}"#),
                http_response("200 OK", r#"{"registry_token":"oidc-reg-jwt"}"#),
            ]
        });
        let env = Arc::new(
            MapEnvSource::new()
                .with("ACTIONS_ID_TOKEN_REQUEST_URL", format!("http://{addr}/id"))
                .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-bearer"),
        );
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            &format!("http://{addr}"),
            "ignored-static-token",
            &fast_policy(),
            env,
        );
        assert_eq!(p.get_token(&log()).unwrap(), "oidc-reg-jwt");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn oidc_appends_audience_with_ampersand_when_url_has_query() {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![
                http_response("200 OK", r#"{"value":"actions-oidc-jwt"}"#),
                http_response("200 OK", r#"{"registry_token":"oidc-reg-jwt"}"#),
            ]
        });
        let env = Arc::new(
            MapEnvSource::new()
                .with(
                    "ACTIONS_ID_TOKEN_REQUEST_URL",
                    format!("http://{addr}/id?api-version=6"),
                )
                .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-bearer"),
        );
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            &format!("http://{addr}"),
            "",
            &fast_policy(),
            env,
        );
        assert_eq!(p.get_token(&log()).unwrap(), "oidc-reg-jwt");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn oidc_empty_id_token_value_errors() {
        let (addr, _calls) =
            spawn_oneshot_http_responder_with(|_| vec![http_response("200 OK", r#"{"value":""}"#)]);
        let env = Arc::new(
            MapEnvSource::new()
                .with("ACTIONS_ID_TOKEN_REQUEST_URL", format!("http://{addr}/id"))
                .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-bearer"),
        );
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            &format!("http://{addr}"),
            "",
            &fast_policy(),
            env,
        );
        let err = p.get_token(&log()).unwrap_err().to_string();
        assert!(
            err.contains("Actions id-token response missing value"),
            "{err}"
        );
    }

    #[test]
    fn oidc_exchange_missing_registry_token_errors() {
        let (addr, _calls) = spawn_oneshot_http_responder_with(|_| {
            vec![
                http_response("200 OK", r#"{"value":"actions-oidc-jwt"}"#),
                http_response("200 OK", "{}"),
            ]
        });
        let env = Arc::new(
            MapEnvSource::new()
                .with("ACTIONS_ID_TOKEN_REQUEST_URL", format!("http://{addr}/id"))
                .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-bearer"),
        );
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            &format!("http://{addr}"),
            "",
            &fast_policy(),
            env,
        );
        let err = p.get_token(&log()).unwrap_err().to_string();
        assert!(
            err.contains("github-oidc response missing registry_token"),
            "{err}"
        );
    }

    #[test]
    fn oidc_id_token_fetch_4xx_fast_fails() {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
            vec![http_response(
                "401 Unauthorized",
                r#"{"error":"bad runner token"}"#,
            )]
        });
        let env = Arc::new(
            MapEnvSource::new()
                .with("ACTIONS_ID_TOKEN_REQUEST_URL", format!("http://{addr}/id"))
                .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-bearer"),
        );
        let p = provider_for_with_env(
            McpAuthMethod::GithubOidc,
            &format!("http://{addr}"),
            "",
            &fast_policy(),
            env,
        );
        let err = format!("{:#}", p.get_token(&log()).unwrap_err());
        assert!(err.contains("HTTP 401"), "{err}");
        assert!(err.contains("bad runner token"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx must not retry");
    }
}
