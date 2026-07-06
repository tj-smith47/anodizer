use anyhow::{Context as _, Result};

use anodizer_core::config::{BlobConfig, RetryConfig};
use anodizer_core::context::Context;
use anodizer_core::template;

use object_store::ObjectStore;
use object_store::RetryConfig as ObjectStoreRetryConfig;

use crate::kms::{KmsProvider, parse_kms_provider};
use crate::provider::Provider;

// ---------------------------------------------------------------------------
// Store construction — one function per provider
// ---------------------------------------------------------------------------

/// Hard cap on `object_store::RetryConfig::retry_timeout`. The upstream
/// docs warn that signed S3/GCS/Azure credentials typically expire on the
/// order of 15 minutes, so a `retry_timeout` longer than ~5 minutes risks
/// the entire retry budget being spent on a request whose credentials
/// silently expired mid-flight. We pin 5 minutes — enough for several
/// exponential-backoff cycles, short enough to fail before the credentials
/// do.
const OBJECT_STORE_RETRY_TIMEOUT_CAP: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Floor on `object_store::RetryConfig::max_retries` for an idempotent blob
/// PUT, decoupled from the global attempt cap. A bucket PUT to a fixed key is
/// idempotent — re-issuing it after a transient 5xx/429 or a dropped
/// connection lands the same bytes at the same key. Stateful modes like
/// `--publish-only` resolve `attempts: 1` → `max_retries: 0`, which strips the
/// SDK's own transient retry entirely and turns a recoverable network blip
/// into a failed release. Flooring at 2 retries (3 total attempts) restores a
/// bounded retry for the recoverable case while a real 4xx (auth/permission)
/// still fails fast inside object_store. Mirrors the HTTP-upload and GitHub
/// asset idempotent-retry floors.
///
/// Sourced from the shared [`anodizer_core::retry::IDEMPOTENT_PUT_ATTEMPTS`]
/// so the "3 total attempts" guarantee is single-sourced. object_store counts
/// RETRIES (not attempts), so the floor is `IDEMPOTENT_PUT_ATTEMPTS - 1`: 2
/// retries == 3 total attempts.
const OBJECT_STORE_MIN_RETRIES: usize = anodizer_core::retry::IDEMPOTENT_PUT_ATTEMPTS as usize - 1;

/// Bridge anodizer's user-facing [`RetryConfig`] (top-level `retry:` block)
/// into [`object_store::RetryConfig`] so the bucket SDK retries align with
/// every other HTTP-uploading publisher. `attempts` includes the first try
/// `attempts` includes the first try, so subtract one to get `max_retries`.
///
/// `retry_timeout` is `min(max_delay × attempts, 5 minutes)`. The naive
/// product (without the cap) yields ~50 minutes for the
/// [`RetryConfig::default`] (5m × 10), well past any signed-URL credential
/// lifetime — see [`OBJECT_STORE_RETRY_TIMEOUT_CAP`].
pub(crate) fn to_object_store_retry(cfg: &RetryConfig) -> ObjectStoreRetryConfig {
    let policy = cfg.to_policy();
    // Idempotent PUTs keep a transient-error retry floor even when a stateful
    // mode (`--publish-only`) resolves `attempts: 1` → `max_retries: 0`.
    let max_retries =
        (policy.max_attempts.saturating_sub(1) as usize).max(OBJECT_STORE_MIN_RETRIES);
    let raw_total = policy.max_delay.saturating_mul(policy.max_attempts.max(1));
    let retry_timeout = std::cmp::min(raw_total, OBJECT_STORE_RETRY_TIMEOUT_CAP);
    ObjectStoreRetryConfig {
        max_retries,
        retry_timeout,
        backoff: object_store::BackoffConfig {
            init_backoff: policy.base_delay,
            max_backoff: policy.max_delay,
            base: 2.0,
        },
    }
}

/// Per-request wall-clock bound applied to every bucket client. Without it an
/// upload whose connection stalls (unreachable endpoint, hung TLS, a black-holed
/// route mid-PUT) hangs the entire release forever — the bucket analogue of the
/// 300 s timeout the gitea/gitlab release backends already carry. `object_store`
/// applies this per HTTP request, so a large multipart upload is bounded
/// per-part, not as a whole.
const OBJECT_STORE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Connect-phase bound applied to every bucket client, so a dead endpoint fails
/// in seconds instead of riding the full request timeout.
const OBJECT_STORE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `ClientOptions` carrying anodizer's standard bucket timeout policy, the
/// plaintext-HTTP allowance, plus any provider-specific default headers (e.g. a
/// canned-ACL header). Every store builder routes through this so no provider's
/// client can be constructed without a request and connect deadline.
///
/// `allow_http` MUST be threaded through here rather than via the builder's
/// standalone `with_allow_http`: `AmazonS3Builder::with_allow_http` writes into
/// `client_options`, and a subsequent `with_client_options(_)` replaces that
/// struct wholesale — so setting `allow_http` on the builder *before* this call
/// is silently discarded, and an `http://` (disable_ssl) endpoint then fails
/// every request with an opaque reqwest "builder error" (scheme not allowed).
/// Owning `allow_http` in the one `ClientOptions` we pass makes it the single
/// source of truth and unclobberable.
fn timed_client_options(
    allow_http: bool,
    headers: Option<reqwest::header::HeaderMap>,
) -> object_store::ClientOptions {
    let mut opts = object_store::ClientOptions::new()
        .with_timeout(OBJECT_STORE_REQUEST_TIMEOUT)
        .with_connect_timeout(OBJECT_STORE_CONNECT_TIMEOUT)
        .with_allow_http(allow_http);
    if let Some(headers) = headers {
        opts = opts.with_default_headers(headers);
    }
    opts
}

/// Build an `ObjectStore` for the given provider and config.
/// All env-based credential chains are handled by the builder's `from_env()`.
pub(crate) fn build_store(
    provider: Provider,
    config: &BlobConfig,
    rendered_bucket: &str,
    ctx: &Context,
) -> Result<Box<dyn ObjectStore>> {
    let retry = ctx.config.retry.unwrap_or_default();
    build_store_with_retry(provider, config, rendered_bucket, ctx, &retry)
}

/// [`build_store`] with an explicit retry policy instead of the config-derived
/// one. The preflight canary uses this with the shallow
/// [`anodizer_core::retry::RetryPolicy::PREFLIGHT`]-derived policy so an
/// unreachable endpoint fails the probe fast instead of riding the full
/// publish-time retry ladder; credentials and addressing
/// (region/endpoint/path-style/SSL/ACL) are resolved identically to the
/// publish path so the probe and the upload talk to the same store.
pub(crate) fn build_store_with_retry(
    provider: Provider,
    config: &BlobConfig,
    rendered_bucket: &str,
    ctx: &Context,
    retry: &RetryConfig,
) -> Result<Box<dyn ObjectStore>> {
    match provider {
        Provider::S3 => build_s3_store(config, rendered_bucket, ctx, retry),
        Provider::Gcs => build_gcs_store(rendered_bucket, config, retry),
        Provider::AzBlob => build_azure_store(rendered_bucket, retry),
    }
}

pub(crate) fn build_s3_store(
    config: &BlobConfig,
    bucket: &str,
    ctx: &Context,
    retry: &RetryConfig,
) -> Result<Box<dyn ObjectStore>> {
    use object_store::aws::AmazonS3Builder;

    let mut builder = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_retry(to_object_store_retry(retry));

    if let Some(ref region) = config.region {
        let rendered = template::render(region, ctx.template_vars())
            .with_context(|| format!("blobs: render region template: {region}"))?;
        builder = builder.with_region(&rendered);
    }

    if let Some(ref endpoint) = config.endpoint {
        let rendered = template::render(endpoint, ctx.template_vars())
            .with_context(|| format!("blobs: render endpoint template: {endpoint}"))?;
        builder = builder.with_endpoint(&rendered);

        // Smart default: force path style when custom endpoint is set.
        // MinIO, R2, DO Spaces, Backblaze B2 all need path-style addressing.
        let force_path = config.s3_force_path_style.unwrap_or(true);
        builder = builder.with_virtual_hosted_style_request(!force_path);
    } else if let Some(force_path) = config.s3_force_path_style {
        builder = builder.with_virtual_hosted_style_request(!force_path);
    }

    // KMS server-side encryption: only set SSE-KMS on the S3 builder when the
    // key is a plain ARN/ID (ServerSide). URL-schemed keys (awskms://, gcpkms://,
    // azurekeyvault://) use client-side encryption — the data is encrypted before
    // upload, so we must NOT also request server-side encryption.
    if let Some(ref kms_key) = config.kms_key
        && parse_kms_provider(kms_key) == KmsProvider::ServerSide
    {
        builder = builder.with_sse_kms_encryption(kms_key);
    }

    // S3 canned ACL via x-amz-acl header.
    // We set it as a default header on the client — since each blob config
    // gets its own ObjectStore client, this is per-config ACL.
    let acl_headers = if let Some(ref acl) = config.acl {
        // Validate against the S3 canned ACL enum — `log-delivery-write`
        // (a valid AWS S3 canned ACL) is omitted to match upstream.
        const VALID_S3_ACLS: &[&str] = &[
            "private",
            "public-read",
            "public-read-write",
            "authenticated-read",
            "aws-exec-read",
            "bucket-owner-read",
            "bucket-owner-full-control",
        ];
        if !VALID_S3_ACLS.contains(&acl.as_str()) {
            anyhow::bail!(
                "blobs: invalid S3 canned ACL '{}'. Valid values are: {}",
                acl,
                VALID_S3_ACLS.join(", ")
            );
        }

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-amz-acl"),
            reqwest::header::HeaderValue::from_str(acl)
                .with_context(|| format!("blobs: invalid ACL value: {acl}"))?,
        );
        Some(headers)
    } else {
        None
    };
    // A plaintext-http endpoint (in-cluster MinIO) MUST allow the http scheme or
    // object_store/reqwest rejects every request pre-flight with an opaque
    // "builder error". Derive it from BOTH the explicit disable_ssl flag AND the
    // endpoint scheme, so a caller that supplies the endpoint but not disable_ssl
    // (the rollback delete path synthesizes a minimal config) still connects.
    // Threaded through the ClientOptions we pass so it cannot be clobbered (see
    // `timed_client_options`).
    let allow_http = config.disable_ssl.unwrap_or(false)
        || config
            .endpoint
            .as_deref()
            .is_some_and(|e| e.starts_with("http://"));
    builder = builder.with_client_options(timed_client_options(allow_http, acl_headers));

    Ok(Box::new(
        builder
            .build()
            .context("blobs: failed to build S3 client")?,
    ))
}

pub(crate) fn build_gcs_store(
    bucket: &str,
    config: &BlobConfig,
    retry: &RetryConfig,
) -> Result<Box<dyn ObjectStore>> {
    use object_store::gcp::GoogleCloudStorageBuilder;

    let mut builder = GoogleCloudStorageBuilder::from_env()
        .with_bucket_name(bucket)
        .with_retry(to_object_store_retry(retry));

    // GCS predefined ACL via x-goog-acl header.
    // Match the public list documented at
    // https://cloud.google.com/storage/docs/access-control/lists#predefined-acl
    // and the GCS XML API canned ACL set so a typo (e.g. `public-read` instead
    // of `publicRead`) errors here rather than producing a 400 deep in upload.
    let acl_headers = if let Some(ref acl) = config.acl {
        const VALID_GCS_ACLS: &[&str] = &[
            "authenticatedRead",
            "bucketOwnerFullControl",
            "bucketOwnerRead",
            "private",
            "projectPrivate",
            "publicRead",
            "publicReadWrite",
        ];
        if !VALID_GCS_ACLS.contains(&acl.as_str()) {
            anyhow::bail!(
                "blobs: invalid GCS predefined ACL '{}'. Valid values are: {}",
                acl,
                VALID_GCS_ACLS.join(", ")
            );
        }
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-goog-acl"),
            reqwest::header::HeaderValue::from_str(acl)
                .with_context(|| format!("blobs: invalid ACL value: {acl}"))?,
        );
        Some(headers)
    } else {
        None
    };
    // GCS is always TLS (no disable_ssl surface), so plaintext HTTP stays off.
    builder = builder.with_client_options(timed_client_options(false, acl_headers));

    Ok(Box::new(
        builder
            .build()
            .context("blobs: failed to build GCS client")?,
    ))
}

pub(crate) fn build_azure_store(
    container: &str,
    retry: &RetryConfig,
) -> Result<Box<dyn ObjectStore>> {
    use object_store::azure::MicrosoftAzureBuilder;

    let builder = MicrosoftAzureBuilder::from_env()
        .with_container_name(container)
        .with_retry(to_object_store_retry(retry))
        .with_client_options(timed_client_options(false, None));

    Ok(Box::new(
        builder
            .build()
            .context("blobs: failed to build Azure Blob client")?,
    ))
}

#[cfg(test)]
mod retry_bridge_tests {
    use super::*;

    #[test]
    fn default_retry_timeout_is_capped_at_five_minutes() {
        // Default is 10 attempts × 5m max_delay = 50m raw, must be capped to 5m
        // so signed-URL credentials can't silently expire mid-retry.
        let bridged = to_object_store_retry(&RetryConfig::default());
        assert_eq!(bridged.retry_timeout, OBJECT_STORE_RETRY_TIMEOUT_CAP);
        assert_eq!(bridged.retry_timeout, std::time::Duration::from_secs(300));
    }

    #[test]
    fn small_retry_budget_passes_through_uncapped() {
        // Below the cap: raw_total wins. 3 × 1s = 3s, well under 5m.
        let cfg = RetryConfig {
            attempts: 3,
            delay: anodizer_core::config::HumanDuration(std::time::Duration::from_secs(1)),
            max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_secs(1)),
            max_elapsed: None,
        };
        let bridged = to_object_store_retry(&cfg);
        assert_eq!(bridged.retry_timeout, std::time::Duration::from_secs(3));
    }

    #[test]
    fn max_retries_subtracts_one_from_attempts() {
        // `attempts` includes the first try; `max_retries` does not.
        let cfg = RetryConfig {
            attempts: 4,
            ..RetryConfig::default()
        };
        assert_eq!(to_object_store_retry(&cfg).max_retries, 3);
    }

    #[test]
    fn max_retries_floored_for_idempotent_put_under_single_attempt() {
        // A stateful mode (`--publish-only`) resolves `attempts: 1`, which
        // would naively yield `max_retries: 0` and strip the SDK's own
        // transient retry from an idempotent PUT. The floor keeps a bounded
        // retry (2) so a recoverable 5xx/429/dropped-connection still re-issues
        // the PUT.
        let cfg = RetryConfig {
            attempts: 1,
            ..RetryConfig::default()
        };
        assert_eq!(
            to_object_store_retry(&cfg).max_retries,
            OBJECT_STORE_MIN_RETRIES
        );
    }

    #[test]
    fn backoff_is_threaded_through() {
        let cfg = RetryConfig {
            attempts: 5,
            delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(250)),
            max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_secs(2)),
            max_elapsed: None,
        };
        let bridged = to_object_store_retry(&cfg);
        assert_eq!(
            bridged.backoff.init_backoff,
            std::time::Duration::from_millis(250)
        );
        assert_eq!(
            bridged.backoff.max_backoff,
            std::time::Duration::from_secs(2)
        );
        assert_eq!(bridged.backoff.base, 2.0);
    }
}

#[cfg(test)]
mod allow_http_regression {
    use super::*;
    use anodizer_core::config::{Config, HumanDuration};
    use anodizer_core::context::ContextOptions;
    use object_store::ObjectStoreExt;

    /// Regression: a `disable_ssl: true` (plaintext-HTTP) endpoint — an
    /// in-cluster MinIO is the canonical case — must have its http scheme
    /// ALLOWED, so a PUT is actually attempted (and fails at the transport
    /// layer against a dead port) rather than rejected pre-flight with an
    /// opaque reqwest "builder error".
    ///
    /// Shared setup for the two dead-port S3 regression tests below: the
    /// AWS_* env dance, `BlobConfig`/`RetryConfig`/`Context` construction,
    /// and the build+PUT are identical between them — only the endpoint
    /// scheme and `disable_ssl` (the config axis each test is actually
    /// pinning) differ. skip_signature avoids an IMDS credential stall and
    /// isolates each test to the behavior it's checking (no creds, no
    /// signing, no network beyond the localhost connect attempt). Callers
    /// must hold `#[serial(aws_env)]` — this mutates the same AWS_* vars a
    /// concurrent test could observe.
    async fn attempt_dead_port_put(
        endpoint: &str,
        disable_ssl: Option<bool>,
    ) -> object_store::Result<object_store::PutResult> {
        unsafe {
            // env-ok: #[serial(aws_env)]; sole mutator of these AWS_* vars
            std::env::set_var("AWS_SKIP_SIGNATURE", "true");
            // env-ok: #[serial(aws_env)]; sole mutator of these AWS_* vars
            std::env::remove_var("AWS_ENDPOINT");
            // env-ok: #[serial(aws_env)]; sole mutator of these AWS_* vars
            std::env::remove_var("AWS_ENDPOINT_URL");
        }
        let config = BlobConfig {
            provider: "s3".into(),
            bucket: "b".into(),
            endpoint: Some(endpoint.into()),
            region: Some("us-east-1".into()),
            s3_force_path_style: Some(true),
            disable_ssl,
            ..Default::default()
        };
        // Tight retry so the dead-port connect fails fast.
        let retry = RetryConfig {
            attempts: 1,
            delay: HumanDuration(std::time::Duration::from_millis(1)),
            max_delay: HumanDuration(std::time::Duration::from_millis(1)),
            max_elapsed: None,
        };
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let store = build_s3_store(&config, "b", &ctx, &retry).expect("store builds");
        let res = store
            .put(&object_store::path::Path::from("k"), b"x".to_vec().into())
            .await;
        unsafe {
            // env-ok: #[serial(aws_env)]; sole mutator of these AWS_* vars
            std::env::remove_var("AWS_SKIP_SIGNATURE");
        }
        res
    }

    /// The bug this pins: `AmazonS3Builder::with_allow_http` writes into
    /// `client_options`, and `build_s3_store`'s later
    /// `with_client_options(timed_client_options(..))` replaced that struct
    /// wholesale — silently reverting `allow_http` to `false`, so every blob
    /// PUT to an http MinIO failed with `HTTP error: builder error`. Threading
    /// `allow_http` through `timed_client_options` makes it unclobberable;
    /// deleting that thread-through makes this test fail with a builder error.
    #[tokio::test]
    #[serial_test::serial(aws_env)]
    async fn disable_ssl_http_endpoint_is_attempted_not_rejected() {
        let res = attempt_dead_port_put("http://127.0.0.1:1", Some(true)).await;
        let err = res.expect_err("a dead port must fail the PUT");
        let msg = err.to_string().to_lowercase();
        assert!(
            !msg.contains("builder error"),
            "disable_ssl http endpoint must be attempted (transport error), \
             never rejected pre-flight as a scheme/builder error: {err}"
        );
    }

    /// Regression: the blob rollback path (`publisher.rs`
    /// `rollback_via_object_store`) synthesizes a minimal `BlobConfig` with
    /// `..Default::default()`, which drops `disable_ssl` even when the
    /// captured endpoint is `http://`. `build_s3_store` must still enable
    /// `allow_http` from the endpoint scheme alone, or every rollback DELETE
    /// against an in-cluster MinIO endpoint is rejected pre-flight with the
    /// same opaque "builder error" the upload-path regression above pins.
    #[tokio::test]
    #[serial_test::serial(aws_env)]
    async fn disable_ssl_none_with_http_endpoint_is_attempted_not_rejected() {
        let res = attempt_dead_port_put("http://127.0.0.1:1", None).await;
        let err = res.expect_err("a dead port must fail the PUT");
        let msg = err.to_string().to_lowercase();
        assert!(
            !msg.contains("builder error"),
            "an http:// endpoint must enable allow_http from the scheme alone, \
             even when disable_ssl is None (the rollback delete path's synthesized \
             config): {err}"
        );
    }

    /// Regression: `object_store`'s `aws`/`gcp`/`azure` features link
    /// `aws-lc-rs` alongside the process-wide `ring` provider installed by
    /// `install_default_crypto_provider()`. rustls 0.23 panics on the first
    /// TLS handshake if two `CryptoProvider`s are linked and neither call
    /// site pins one explicitly ("Could not automatically determine the
    /// process-level CryptoProvider"). Driving a real handshake attempt
    /// (an https endpoint that fails at the transport layer, same shape as
    /// `disable_ssl_http_endpoint_is_attempted_not_rejected`) after installing
    /// `ring` pins that no panic reaches the caller — only an ordinary
    /// transport error.
    #[tokio::test]
    #[serial_test::serial(aws_env)]
    async fn crypto_provider_coexistence_does_not_panic_on_handshake() {
        anodizer_core::tls::install_default_crypto_provider();
        let res = attempt_dead_port_put("https://127.0.0.1:1", None).await;
        // A `CryptoProvider` auto-select panic would abort this call, not
        // return an `Err` — the assertion below only runs at all if no panic
        // occurred.
        res.expect_err("a dead port must fail the PUT at the transport layer, not panic");
    }
}
