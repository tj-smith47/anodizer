//! HTTP client helpers shared by every stage that talks to a remote.
//!
//! All anodizer HTTP traffic should go through `blocking_client(...)` so that
//! the `User-Agent`, default-roots, and timeout policy stay consistent across
//! publishers, announcers, and the release backends.

use std::time::Duration;

use anyhow::{Context as _, Result};

/// Canonical user-agent string sent with every anodizer HTTP request.
///
/// Versioning the UA matters for upstream services that rate-limit or
/// fingerprint by client identity (Discourse, Reddit, GitHub, etc.).
pub const USER_AGENT: &str = concat!("anodizer/", env!("CARGO_PKG_VERSION"));

/// Build a blocking `reqwest::Client` configured with the canonical UA,
/// the requested per-request timeout, and the platform's built-in roots.
pub fn blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
        .context("build blocking HTTP client")
}

/// [`blocking_client`] with redirect-following disabled: a 3xx response is
/// returned to the caller instead of being chased. For probes whose verdict
/// depends on whether a SPECIFIC URL resolves (e.g. a registry version page),
/// silently following a redirect to a different page would misattribute the
/// destination's body to the requested resource.
pub fn blocking_client_no_redirect(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build blocking HTTP client (no redirects)")
}

/// Async equivalent of `blocking_client`.
pub fn async_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
        .context("build async HTTP client")
}

/// Resolve the GitHub REST API base URL through an injected env source.
///
/// Honors the undocumented `ANODIZER_GITHUB_API_BASE` override so unit tests
/// can redirect GitHub REST calls to an in-process responder via a
/// [`MapEnvSource`](crate::MapEnvSource); defaults to the canonical
/// `https://api.github.com` in production where callers pass
/// [`ProcessEnvSource`](crate::ProcessEnvSource) and the var is unset. Any
/// trailing `/` is stripped so callers can unconditionally `format!` with a
/// `/`-prefixed suffix without producing a double slash.
///
/// Every GitHub REST caller (release-stage rate-limit polls, publish-stage
/// default-branch / PR lookups) must resolve its base through this one
/// helper so a single override redirects the whole run to the same host.
pub fn github_api_base<E: crate::EnvSource + ?Sized>(env: &E) -> String {
    let raw = env
        .var("ANODIZER_GITHUB_API_BASE")
        .unwrap_or_else(|| "https://api.github.com".to_string());
    raw.trim_end_matches('/').to_string()
}

/// Like [`github_api_base`], but honoring a configured `github_urls.api`
/// (GitHub Enterprise Server) first.
///
/// Preflight probes and milestone operations must contact the same host the
/// release backend will (see `build_octocrab_client`): probing github.com
/// for a repo that lives on a GHES host false-404s (Blocker for a release
/// that would succeed), or worse, returns a verdict for an unrelated
/// same-named public repo. Precedence: `github_urls.api` >
/// `ANODIZER_GITHUB_API_BASE` > `https://api.github.com`. Any trailing `/`
/// is stripped, as in [`github_api_base`].
pub fn github_api_base_with_config<E: crate::EnvSource + ?Sized>(
    github_urls: Option<&crate::config::GitHubUrlsConfig>,
    env: &E,
) -> String {
    github_urls
        .and_then(|u| u.api.as_deref())
        .map(|api| api.trim_end_matches('/').to_string())
        .unwrap_or_else(|| github_api_base(env))
}

/// Format an HTTP body-read failure as a descriptive placeholder string.
///
/// Used by [`body_of`] / [`body_of_blocking`]: a transport-level
/// read error becomes `"could not read response body: <err>"` rather than
/// silently truncating to `""`. Exposed as a free function so unit tests can
/// pin the exact wording without standing up a fault-injecting HTTP server.
pub fn body_read_error_message<E: std::fmt::Display>(err: E) -> String {
    format!("could not read response body: {err}")
}

/// Read an HTTP response body to a `String`, returning a descriptive
/// placeholder on read failure.
///
/// Reads and scrubs an HTTP response body for error reporting after
/// commit `8b77358`: a transport-level read error becomes
/// `"could not read response body: <err>"` rather than silently truncating
/// to an empty string. Callers typically pass the resulting text into a
/// larger error context (e.g. `"GitHub API returned 422: {body}"`), so the
/// placeholder still surfaces a usable diagnostic instead of a confusing
/// empty payload.
///
/// Use this when the body will be interpolated into a downstream error
/// message; use `resp.text().await?` directly when the caller will
/// propagate the read failure as its own error rather than substituting
/// a placeholder.
pub async fn body_of(resp: reqwest::Response) -> String {
    match resp.text().await {
        Ok(s) => s,
        Err(err) => body_read_error_message(err),
    }
}

/// Blocking analogue of [`body_of`].
///
/// Use this when the body will be interpolated into a downstream error
/// message; use `resp.text()?` directly when the caller will propagate
/// the read failure as its own error rather than substituting a
/// placeholder.
pub fn body_of_blocking(resp: reqwest::blocking::Response) -> String {
    match resp.text() {
        Ok(s) => s,
        Err(err) => body_read_error_message(err),
    }
}

/// Download `url` (blocking, 5-minute timeout) and return the lowercase-hex
/// SHA-256 of its body — the canonical "fetch a release artifact, hash it"
/// helper for publishers that must fill a digest they were not handed (e.g.
/// the homebrew-core formula bump when no `sha256:` override is configured).
///
/// The 5-minute timeout accommodates multi-MB release tarballs; a non-2xx
/// response is a hard error carrying the status and body so the caller need
/// not re-classify.
pub fn sha256_url(url: &str) -> Result<String> {
    use sha2::Digest as _;
    let client = blocking_client(Duration::from_secs(300)).context("build download client")?;
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("download {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!(
            "download {} returned HTTP {}: {}",
            url,
            status,
            body_of_blocking(resp)
        );
    }
    let bytes = resp
        .bytes()
        .with_context(|| format!("read download body from {url}"))?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    Ok(crate::hashing::hex_lower(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_api_base_strips_trailing_slash() {
        let env =
            crate::MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", "https://example.com/api/");
        assert_eq!(github_api_base(&env), "https://example.com/api");
    }

    #[test]
    fn github_api_base_defaults_when_env_unset() {
        let env = crate::MapEnvSource::new();
        assert_eq!(github_api_base(&env), "https://api.github.com");
    }

    #[test]
    fn github_api_base_with_config_prefers_configured_ghes_api() {
        let urls = crate::config::GitHubUrlsConfig {
            api: Some("https://github.example.com/api/v3/".to_string()),
            ..Default::default()
        };
        let env =
            crate::MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", "https://override.test");
        assert_eq!(
            github_api_base_with_config(Some(&urls), &env),
            "https://github.example.com/api/v3"
        );
    }

    #[test]
    fn github_api_base_with_config_falls_back_to_env_resolver() {
        let env =
            crate::MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", "https://override.test/");
        assert_eq!(
            github_api_base_with_config(None, &env),
            "https://override.test"
        );
        let unset = crate::MapEnvSource::new();
        assert_eq!(
            github_api_base_with_config(None, &unset),
            "https://api.github.com"
        );
    }

    #[test]
    fn test_body_read_error_message_uses_descriptive_prefix() {
        // Pin the exact wording: callers may parse / match on this string,
        // and the error-body contract requires the
        // `"could not read response body: "` prefix verbatim.
        let formatted = body_read_error_message("connection reset by peer");
        assert_eq!(
            formatted,
            "could not read response body: connection reset by peer"
        );
    }

    #[test]
    fn test_body_read_error_message_with_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "stream ended early");
        let formatted = body_read_error_message(io_err);
        assert!(
            formatted.starts_with("could not read response body: "),
            "format must keep the descriptive prefix: {formatted}"
        );
        assert!(
            formatted.contains("stream ended early"),
            "format must include the underlying error: {formatted}"
        );
    }
}
