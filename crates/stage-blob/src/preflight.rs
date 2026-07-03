//! Live object-store probe for the blob publisher's
//! [`anodizer_core::Publisher::preflight`].
//!
//! blob is an Assets-group publisher that uploads release archives to a
//! cloud object store (S3 / GCS / Azure, including self-hosted S3-compatible
//! mirrors like MinIO). A broken or unreachable store discovered only at
//! upload time fails AFTER the one-way-door publishers (cargo / chocolatey /
//! winget) have already fired. This module runs the probe at preflight —
//! before any tag is cut — by constructing the SAME store the publish uses
//! and performing a minimal PUT → HEAD → DELETE canary round-trip against
//! each deduplicated destination. Any error blocks (when the destination's
//! config is `required: true`) or warns (otherwise), mirroring how a runtime
//! failure gates the release.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodizer_core::PreflightCheck;
use anodizer_core::config::{BlobConfig, HumanDuration, RetryConfig};
use anodizer_core::context::Context;
use anodizer_core::retry::RetryPolicy;

use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};

use crate::provider::Provider;
use crate::store::build_store_with_retry;

/// Hard wall-clock ceiling on one endpoint's canary round-trip so a
/// black-holed route or hung TLS converts into a probe error instead of
/// hanging the preflight gate. Exceeds the client connect timeout (30s) so a
/// refused / unreachable endpoint surfaces its real error first.
const PREFLIGHT_PROBE_TIMEOUT: Duration = Duration::from_secs(45);

/// Bytes PUT by the canary. Tiny and fixed — the probe validates
/// reachability + auth + write/read/delete permission, not throughput.
const CANARY_PAYLOAD: &[u8] = b"anodizer-blob-preflight";

/// Deduplication key for an object-store destination: the addressing tuple a
/// store handle is uniquely defined by.
type TargetKey = (String, String, Option<String>, Option<String>);

/// One deduplicated destination the publish would write to.
struct ProbeTarget {
    provider: Provider,
    provider_str: String,
    bucket: String,
    endpoint: Option<String>,
    /// True when any blob config addressing this destination set
    /// `required: true` — selects Blocker (required) vs Warning (optional)
    /// on a probe failure, matching the runtime gate weight.
    required: bool,
    /// The blob config whose addressing fields (region / endpoint /
    /// path-style / SSL / ACL / KMS) `build_store_with_retry` reads. Passed
    /// verbatim — exactly as the publish path passes it — so the probe and
    /// the upload construct an identical store.
    cfg: BlobConfig,
}

/// Run the blob publisher's preflight probe. See the module docs for the
/// round-trip shape and the Blocker-vs-Warning rule.
pub(crate) fn run_preflight(ctx: &Context) -> Result<PreflightCheck> {
    // Deselected (`--skip=blob` / omitted from `--publishers`): nothing will
    // upload, so there is nothing to probe.
    if ctx.publisher_deselected("blob") {
        return Ok(PreflightCheck::Pass);
    }

    let mut acc = PreflightCheck::Pass;
    let targets = collect_targets(ctx, &mut acc)?;
    if targets.is_empty() {
        // No configured destination (or every config gated off / rendered
        // indeterminate): pass, carrying any render-indeterminate warning.
        return Ok(acc);
    }

    // One current-thread runtime for the whole probe; the canary is a few
    // sequential round-trips, no need for a worker pool.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("blob preflight: failed to construct tokio runtime")?;

    for target in targets.into_values() {
        let label = target_label(&target);
        let result = probe_target(ctx, &rt, &target);
        acc = merge(acc, classify_probe(&label, target.required, result));
    }
    Ok(acc)
}

/// Walk the same crate set the publish path iterates (the crate universe
/// filtered by `--crate` selection), render each active blob config's
/// addressing, and dedupe into `(provider, bucket, region, endpoint)`
/// destinations. A render failure for an addressing field is indeterminate
/// at preflight (the publish renders it later with the real tag) and merges
/// a Warning rather than a false Blocker.
fn collect_targets(
    ctx: &Context,
    acc: &mut PreflightCheck,
) -> Result<BTreeMap<TargetKey, ProbeTarget>> {
    let selected = &ctx.options.selected_crates;
    let mut targets: BTreeMap<TargetKey, ProbeTarget> = BTreeMap::new();

    for krate in ctx
        .config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
    {
        let Some(blob_configs) = krate.blobs.as_ref() else {
            continue;
        };
        for blob_cfg in blob_configs {
            if config_inactive(ctx, blob_cfg)? {
                continue;
            }
            // Empty required fields fail the validate stage with a precise
            // error; the probe has no store to build, so skip rather than
            // duplicate that diagnostic here.
            if blob_cfg.provider.is_empty() || blob_cfg.bucket.is_empty() {
                continue;
            }

            let provider_str = match ctx.render_template(&blob_cfg.provider) {
                Ok(s) => s,
                Err(e) => {
                    *acc = merge(
                        acc.clone(),
                        PreflightCheck::Warning(format!(
                            "blob preflight: could not render provider for crate '{}' ({e:#}); \
                             skipping live probe for this destination",
                            krate.name
                        )),
                    );
                    continue;
                }
            };
            // An unknown provider is a validate-stage error, not a
            // reachability concern — leave that diagnostic to validate.
            let Ok(provider) = Provider::parse(&provider_str) else {
                continue;
            };
            let Some(bucket) = render_addr_field(
                ctx,
                acc,
                &krate.name,
                "bucket",
                Some(blob_cfg.bucket.as_str()),
            )?
            else {
                continue;
            };
            let bucket = bucket.expect("bucket addr field is Some");
            let Some(region) =
                render_addr_field(ctx, acc, &krate.name, "region", blob_cfg.region.as_deref())?
            else {
                continue;
            };
            let Some(endpoint) = render_addr_field(
                ctx,
                acc,
                &krate.name,
                "endpoint",
                blob_cfg.endpoint.as_deref(),
            )?
            else {
                continue;
            };

            let required = blob_cfg.required.unwrap_or(false);
            let key: TargetKey = (
                provider_str.clone(),
                bucket.clone(),
                region.clone(),
                endpoint.clone(),
            );
            targets
                .entry(key)
                .and_modify(|t| t.required |= required)
                .or_insert_with(|| ProbeTarget {
                    provider,
                    provider_str,
                    bucket,
                    endpoint,
                    required,
                    cfg: blob_cfg.clone(),
                });
        }
    }
    Ok(targets)
}

/// Render one optional addressing field (`bucket` / `region` / `endpoint`).
///
/// Return shape:
/// - `Ok(Some(Some(s)))` — field set and rendered to `s`.
/// - `Ok(Some(None))`    — field absent (no render attempted).
/// - `Ok(None)`          — render failed; an indeterminate Warning was merged
///   into `acc` and the caller skips this destination.
fn render_addr_field(
    ctx: &Context,
    acc: &mut PreflightCheck,
    crate_name: &str,
    field: &str,
    raw: Option<&str>,
) -> Result<Option<Option<String>>> {
    let Some(raw) = raw else {
        return Ok(Some(None));
    };
    match ctx.render_template(raw) {
        Ok(s) => Ok(Some(Some(s))),
        Err(e) => {
            *acc = merge(
                acc.clone(),
                PreflightCheck::Warning(format!(
                    "blob preflight: could not render {field} for crate '{crate_name}' ({e:#}); \
                     skipping live probe for this destination"
                )),
            );
            Ok(None)
        }
    }
}

/// Whether a blob config is gated off for this run via `skip:` or a falsy
/// `if:` condition. Evaluated without logging — preflight is a silent gate.
fn config_inactive(ctx: &Context, blob_cfg: &BlobConfig) -> Result<bool> {
    if let Some(skip) = blob_cfg.skip.as_ref()
        && skip.try_evaluates_to_true(|s| ctx.render_template(s))?
    {
        return Ok(true);
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        blob_cfg.if_condition.as_deref(),
        "blob preflight",
        |t| ctx.render_template(t),
    )?;
    Ok(!proceed)
}

/// `provider://bucket` (plus an endpoint hint for S3-compatible mirrors) for
/// the probe's actionable message.
fn target_label(t: &ProbeTarget) -> String {
    match t.endpoint.as_deref() {
        Some(ep) if !ep.is_empty() => format!("{}://{} (endpoint {ep})", t.provider_str, t.bucket),
        _ => format!("{}://{}", t.provider_str, t.bucket),
    }
}

/// Build the publish store (shallow preflight retry policy) and run the
/// canary against `target`.
fn probe_target(ctx: &Context, rt: &tokio::runtime::Runtime, target: &ProbeTarget) -> Result<()> {
    // Shallow retry so an unreachable endpoint fails fast instead of riding
    // the full publish-time retry ladder. Same source-of-truth policy the
    // homebrew / nix / scoop preflights use.
    let policy = RetryPolicy::PREFLIGHT;
    let retry = RetryConfig {
        attempts: policy.max_attempts,
        delay: HumanDuration(policy.base_delay),
        max_delay: HumanDuration(policy.max_delay),
    };
    let store = build_store_with_retry(target.provider, &target.cfg, &target.bucket, ctx, &retry)
        .context("construct object store")?;
    let key = canary_key();
    canary_roundtrip(rt, store.as_ref(), &key)
}

/// A unique, unobtrusive canary key at the bucket root. The nanosecond stamp
/// keeps concurrent / repeated preflights from colliding; the object is
/// deleted before the probe returns.
fn canary_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".anodizer-preflight-{nanos}")
}

/// PUT a canary object, HEAD it, then DELETE it, bounded by
/// [`PREFLIGHT_PROBE_TIMEOUT`]. Exercises the same write / read / delete
/// permissions the publish + rollback paths need. DELETE is attempted even
/// when HEAD fails so a successful PUT never leaves a stray object behind.
fn canary_roundtrip(
    rt: &tokio::runtime::Runtime,
    store: &dyn ObjectStore,
    key: &str,
) -> Result<()> {
    let path = ObjectPath::from(key);
    rt.block_on(async {
        tokio::time::timeout(PREFLIGHT_PROBE_TIMEOUT, async {
            let payload: object_store::PutPayload = CANARY_PAYLOAD.to_vec().into();
            store
                .put(&path, payload)
                .await
                .with_context(|| format!("PUT canary {key} failed"))?;

            let head = store.head(&path).await;
            // Best-effort cleanup regardless of the HEAD verdict.
            let delete = store.delete(&path).await;

            head.with_context(|| format!("HEAD canary {key} failed"))?;
            delete.with_context(|| format!("DELETE canary {key} failed"))?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "canary round-trip exceeded {}s",
                PREFLIGHT_PROBE_TIMEOUT.as_secs()
            )
        })?
    })
}

/// Map a probe outcome to a [`PreflightCheck`]. A failure to a `required`
/// destination Blocks (the publish would fail the release); to an optional
/// one it Warns (a runtime failure there is non-gating).
fn classify_probe(label: &str, required: bool, result: Result<()>) -> PreflightCheck {
    match result {
        Ok(()) => PreflightCheck::Pass,
        Err(e) => {
            let msg = format!("blob preflight: object-store probe to {label} failed: {e:#}");
            if required {
                PreflightCheck::Blocker(msg)
            } else {
                PreflightCheck::Warning(msg)
            }
        }
    }
}

/// Keep the most severe of two checks (Blocker > Warning > Pass), preserving
/// the first message at a given severity.
fn merge(acc: PreflightCheck, next: PreflightCheck) -> PreflightCheck {
    acc.merge(next)
}

#[cfg(test)]
mod preflight_tests {
    use super::*;
    use anodizer_core::config::{BlobConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn blob_crate(name: &str, cfg: BlobConfig) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            blobs: Some(vec![cfg]),
            ..Default::default()
        }
    }

    #[test]
    fn canary_roundtrip_succeeds_and_cleans_up_on_in_memory_store() {
        let store = object_store::memory::InMemory::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let key = canary_key();
        canary_roundtrip(&rt, &store, &key).expect("round-trip succeeds against in-memory store");
        // The canary must be gone — DELETE ran as part of the round-trip.
        let head = rt.block_on(store.head(&ObjectPath::from(key.as_str())));
        assert!(
            matches!(head, Err(object_store::Error::NotFound { .. })),
            "canary object should be deleted after a successful round-trip; got {head:?}"
        );
    }

    #[test]
    fn classify_probe_passes_on_success() {
        assert_eq!(classify_probe("s3://b", true, Ok(())), PreflightCheck::Pass);
    }

    #[test]
    fn classify_probe_blocks_required_failure() {
        let check = classify_probe(
            "s3://releases (endpoint http://minio:9000)",
            true,
            Err(anyhow::anyhow!("connection refused")),
        );
        match check {
            PreflightCheck::Blocker(m) => {
                assert!(m.contains("s3://releases"), "names the destination: {m}");
                assert!(m.contains("minio:9000"), "names the endpoint: {m}");
                assert!(m.contains("connection refused"), "names the cause: {m}");
            }
            other => panic!("required probe failure must Block, got {other:?}"),
        }
    }

    #[test]
    fn classify_probe_warns_optional_failure() {
        let check = classify_probe("s3://b", false, Err(anyhow::anyhow!("boom")));
        assert!(
            matches!(check, PreflightCheck::Warning(_)),
            "optional probe failure must Warn, got {check:?}"
        );
    }

    #[test]
    fn preflight_passes_without_blob_config() {
        let ctx = TestContextBuilder::new().build();
        assert_eq!(
            run_preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        );
    }

    #[test]
    fn preflight_passes_when_deselected_without_probing() {
        // A reachable endpoint is impossible in a unit test, so a deselected
        // run that returns Pass proves the probe was skipped (a live probe
        // would have errored).
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "releases".to_string(),
            endpoint: Some("http://127.0.0.1:1".to_string()),
            required: Some(true),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .crates(vec![blob_crate("app", cfg)])
            .build();
        ctx.options.skip_stages.push("blob".to_string());
        assert_eq!(
            run_preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        );
    }

    #[test]
    fn collect_targets_dedupes_identical_destinations() {
        let cfg = || BlobConfig {
            provider: "s3".to_string(),
            bucket: "releases".to_string(),
            endpoint: Some("http://minio:9000".to_string()),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![cfg(), cfg()]),
                ..Default::default()
            }])
            .build();
        let mut acc = PreflightCheck::Pass;
        let targets = collect_targets(&ctx, &mut acc).expect("collect ok");
        assert_eq!(
            targets.len(),
            1,
            "identical destinations collapse to one probe"
        );
    }

    #[test]
    fn collect_targets_required_is_ored_across_configs() {
        let optional = BlobConfig {
            provider: "s3".to_string(),
            bucket: "releases".to_string(),
            endpoint: Some("http://minio:9000".to_string()),
            required: Some(false),
            ..Default::default()
        };
        let required = BlobConfig {
            required: Some(true),
            ..optional.clone()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![optional, required]),
                ..Default::default()
            }])
            .build();
        let mut acc = PreflightCheck::Pass;
        let targets = collect_targets(&ctx, &mut acc).expect("collect ok");
        assert_eq!(targets.len(), 1);
        assert!(
            targets.into_values().next().expect("one target").required,
            "a required config must mark the shared destination required"
        );
    }
}
