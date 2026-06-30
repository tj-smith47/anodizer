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

/// `ClientOptions` carrying anodizer's standard bucket timeout policy, plus any
/// provider-specific default headers (e.g. a canned-ACL header). Every store
/// builder routes through this so no provider's client can be constructed
/// without a request and connect deadline.
fn timed_client_options(
    headers: Option<reqwest::header::HeaderMap>,
) -> object_store::ClientOptions {
    let mut opts = object_store::ClientOptions::new()
        .with_timeout(OBJECT_STORE_REQUEST_TIMEOUT)
        .with_connect_timeout(OBJECT_STORE_CONNECT_TIMEOUT);
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

    if config.disable_ssl.unwrap_or(false) {
        builder = builder.with_allow_http(true);
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
    builder = builder.with_client_options(timed_client_options(acl_headers));

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
    builder = builder.with_client_options(timed_client_options(acl_headers));

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
        .with_client_options(timed_client_options(None));

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
