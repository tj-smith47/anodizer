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

/// Async equivalent of `blocking_client`.
pub fn async_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
        .context("build async HTTP client")
}

/// Format an HTTP body-read failure as a descriptive placeholder string.
///
/// Used by [`body_of`] / [`body_of_blocking`] to mirror upstream GoReleaser's
/// `internal/client/github.go::bodyOf` (commit `8b77358`): a transport-level
/// read error becomes `"could not read response body: <err>"` rather than
/// silently truncating to `""`. Exposed as a free function so unit tests can
/// pin the exact wording without standing up a fault-injecting HTTP server.
pub fn format_body_read_error<E: std::fmt::Display>(err: E) -> String {
    format!("could not read response body: {err}")
}

/// Read an HTTP response body to a `String`, returning a descriptive
/// placeholder on read failure.
///
/// Mirrors GoReleaser's `internal/client/github.go::bodyOf` after upstream
/// commit `8b77358`: a transport-level read error becomes
/// `"could not read response body: <err>"` rather than silently truncating
/// to an empty string. Callers typically pass the resulting text into a
/// larger error context (e.g. `"GitHub API returned 422: {body}"`), so the
/// placeholder still surfaces a usable diagnostic instead of a confusing
/// empty payload.
pub async fn body_of(resp: reqwest::Response) -> String {
    match resp.text().await {
        Ok(s) => s,
        Err(err) => format_body_read_error(err),
    }
}

/// Blocking analogue of [`body_of`].
pub fn body_of_blocking(resp: reqwest::blocking::Response) -> String {
    match resp.text() {
        Ok(s) => s,
        Err(err) => format_body_read_error(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_body_read_error_uses_descriptive_prefix() {
        // Pin the exact wording: callers may parse / match on this string,
        // and parity with upstream GoReleaser's `bodyOf` requires the
        // `"could not read response body: "` prefix verbatim.
        let formatted = format_body_read_error("connection reset by peer");
        assert_eq!(
            formatted,
            "could not read response body: connection reset by peer"
        );
    }

    #[test]
    fn test_format_body_read_error_with_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "stream ended early");
        let formatted = format_body_read_error(io_err);
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
