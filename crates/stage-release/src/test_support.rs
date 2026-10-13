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
    // `OctocrabBuilder::new()` builds octocrab's internal reqwest client, which
    // resolves the process-default rustls `CryptoProvider`. The graph links
    // both `ring` and `aws-lc-rs`, so rustls panics on auto-selection unless a
    // default is installed — and under nextest each test runs in its own
    // process, so no earlier test's install carries over. Install `ring` here;
    // idempotent.
    anodizer_core::tls::install_default_crypto_provider();

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

/// Synthesize an `octocrab::Error::GitHub` for a chosen status and body.
///
/// octocrab's `*Snafu` builders are private, so the canonical path to
/// a typed `Error::GitHub { source, .. }` is to drive a real HTTP
/// response through the client and capture the `Err`. For 429 / 5xx
/// responses octocrab's tower retry middleware makes up to 3
/// additional attempts, so the responder must serve the response
/// 4 times for those statuses; for other 4xx statuses 1 is enough.
pub(crate) async fn synth_github_error(status: u16, body: &str) -> octocrab::Error {
    let body_len = body.len();
    let raw = format!(
        "HTTP/1.1 {status} STATUS\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {body_len}\r\n\
         \r\n\
         {body}"
    );
    let static_resp: &'static str = Box::leak(raw.into_boxed_str());
    let serve_count: usize = if status == 429 || status >= 500 { 4 } else { 1 };
    let (addr, _calls) =
        anodizer_core::test_helpers::responder::spawn_oneshot_http_responder(vec![
            static_resp;
            serve_count
        ]);
    let octo = build_test_octocrab(addr);
    octo.get::<serde_json::Value, _, _>("/test", None::<&()>)
        .await
        .expect_err("synth_github_error: octocrab must surface Err for non-2xx status")
}
