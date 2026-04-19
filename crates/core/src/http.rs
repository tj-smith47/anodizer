//! HTTP client helpers shared by every stage that talks to a remote.
//!
//! All anodize HTTP traffic should go through `blocking_client(...)` so that
//! the `User-Agent`, default-roots, and timeout policy stay consistent across
//! publishers, announcers, and the release backends.

use std::time::Duration;

use anyhow::{Context as _, Result};

/// Canonical user-agent string sent with every anodize HTTP request.
///
/// Versioning the UA matters for upstream services that rate-limit or
/// fingerprint by client identity (Discourse, Reddit, GitHub, etc.).
pub const USER_AGENT: &str = concat!("anodize/", env!("CARGO_PKG_VERSION"));

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
