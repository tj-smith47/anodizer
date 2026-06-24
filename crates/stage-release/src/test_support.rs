//! Shared test fixtures for the stage-release crate.
//!
//! Lifted from inline helpers that previously lived in single test
//! modules so siblings (assets.rs, github/mod.rs, rate_limit.rs,
//! release_body.rs, etc.) can reuse them.

#![cfg(test)]

use std::net::SocketAddr;
use std::sync::Arc;

/// Build an `Arc<octocrab::Octocrab>` pointed at a loopback address —
/// pair with [`anodizer_core::test_helpers::responder::spawn_oneshot_http_responder`]
/// to exercise any `Octocrab` call against a scripted HTTP fixture.
///
/// `Arc` matches the production signatures (assets.rs et al. take
/// `&Arc<Octocrab>`); for callers that need the bare client, `.clone()`
/// or `Arc::clone` after this returns.
pub(crate) fn build_test_octocrab(addr: SocketAddr) -> Arc<octocrab::Octocrab> {
    let builder = octocrab::OctocrabBuilder::new()
        .base_uri(format!("http://{addr}/"))
        .expect("OctocrabBuilder::base_uri accepts loopback URL");
    Arc::new(
        builder
            .build()
            .expect("OctocrabBuilder::build succeeds on loopback URL"),
    )
}

/// A tiny [`RetryPolicy`](anodizer_core::retry::RetryPolicy) tuned for
/// tests — high attempt count but millisecond-scale delays so the
/// retry-through-success path resolves quickly without the production
/// 5 s defaults. Use this anywhere a test wants to exercise the retry
/// loop without padding the run with real seconds of sleep.
#[allow(dead_code)]
pub(crate) fn test_retry_policy() -> anodizer_core::retry::RetryPolicy {
    anodizer_core::retry::RetryPolicy {
        max_attempts: 5,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(2),
    }
}
