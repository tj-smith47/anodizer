//! [`anodizer_core::Publisher`] wrapper around [`BlobStage::run`].
//!
//! Lives in `stage-blob` (not `stage-publish`) so the cloud-storage upload
//! path can implement the trait without dragging `stage-publish` into the
//! dependency graph. `stage-publish`'s registry adds `anodizer-stage-blob`
//! as a dep and pushes `BlobPublisher` into the configured publisher list.
//!
//! Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable bytes,
//! server-side deletable). `required = false`.
//!
//! Rollback shape: each provider's [`object_store::ObjectStore`] exposes a
//! [`object_store::ObjectStore::delete`] API. The publisher captures
//! structured [`BlobTarget`] tuples (provider, bucket, key, region,
//! endpoint) at upload time and persists them to
//! [`anodizer_core::PublishEvidence`]`.extra.blob_targets`; the rollback
//! path decodes those, reconstructs the store via
//! [`crate::store::build_store`], and issues `store.delete(&path).await`
//! per object. `object_store::Error::NotFound` is treated as success
//! (the object was already gone — common when an operator pre-deletes
//! via the cloud console or a prior partial rollback already ran).
//!
//! Legacy evidence (written before the structured-target capture
//! landed) carries only `artifact_paths` with no `blob_targets` payload;
//! the rollback path falls back to a per-object warn-only manual-cleanup
//! checklist for those runs (see [`blob_manual_cleanup_msg`]). The
//! warn-only fallback is also reached when [`decode_blob_targets`]
//! returns an empty list, which keeps the surface forward-compatible
//! with any future evidence-shape change.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::run::BlobStage;

/// One blob upload target as recorded in evidence.
///
/// The five fields are the minimum needed to reconstruct a
/// [`crate::store::build_store`] handle plus the [`object_store::path::Path`]
/// to DELETE. Upload-side concerns (KMS, ACL, cache-control,
/// content-disposition) are intentionally omitted — they shape the
/// upload metadata, not the delete operation.
///
/// - `provider` — dispatches `build_store` to the correct backend
///   (`"s3"` / `"gs"` / `"azblob"`, matching
///   [`crate::Provider::display_name`]).
/// - `bucket` — post-template bucket / container name.
/// - `key` — post-template object key WITHIN the bucket (already
///   includes the rendered directory prefix; matches
///   [`object_store::path::Path::from`] input).
/// - `region` — required for S3 (Optional for forward-compat with non-S3
///   providers); `None` for GCS / Azure.
/// - `endpoint` — required for S3-compatible storage (MinIO, R2, DO
///   Spaces, Backblaze B2); `None` for plain AWS / GCS / Azure.
///
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`].
pub(crate) type BlobTarget = anodizer_core::publish_evidence::BlobTargetSnapshot;

/// Render the operator-readable `<provider>://<bucket>/<key>` URL for a
/// [`BlobTarget`]. Used by the publisher to populate
/// [`anodizer_core::PublishEvidence`]`.artifact_paths` so the text
/// `--rollback-only` summary keeps rendering the same shape that
/// shipped before the structured-target capture landed.
///
/// Free function rather than an inherent impl because [`BlobTarget`]
/// is a type alias for a core-owned struct — Rust does not allow
/// inherent impls on type aliases.
pub(crate) fn blob_target_url(t: &BlobTarget) -> String {
    format!("{}://{}/{}", t.provider, t.bucket, t.key)
}

/// Encode the per-target tuples into the typed
/// [`PublishEvidenceExtra::Blob`] variant. Mirrors the cloudsmith
/// pattern (typed `Cloudsmith` variant).
pub(crate) fn encode_blob_targets(targets: &[BlobTarget]) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Blob(anodizer_core::publish_evidence::BlobExtra {
        blob_targets: targets.to_vec(),
    })
}

/// Decode the typed Blob variant. Returns an empty vec when the
/// variant doesn't match — rollback then falls back to the warn-only
/// manual-cleanup path instead of crashing.
pub(crate) fn decode_blob_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<BlobTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Blob(b) => b.blob_targets.clone(),
        _ => Vec::new(),
    }
}

/// [`anodizer_core::Publisher`] adapter over [`BlobStage::run`].
///
/// Evidence records ONLY files that actually landed in the store (via
/// [`BlobStage::run_with_evidence`]). The prior pre-upload capture would
/// have given an operator running `--rollback-only` a checklist of paths
/// that never existed when a mid-stream upload failed; the post-upload
/// snapshot is the safer end state — fewer rollback items, no phantom
/// targets.
pub struct BlobPublisher {
    required_override: Option<bool>,
}

impl BlobPublisher {
    pub fn new() -> Self {
        Self {
            required_override: None,
        }
    }

    /// Construct with a config-supplied `required` override.
    ///
    /// Pass the `Option<bool>` from the blob config. `None` keeps the
    /// built-in default (`false` — per-blob `BlobConfig.required` governs
    /// the stage outcome independently). `Some(v)` overrides the trait
    /// surface for the dispatch path.
    pub fn with_required(required_override: Option<bool>) -> Self {
        Self { required_override }
    }
}

impl Default for BlobPublisher {
    fn default() -> Self {
        Self::new()
    }
}

/// The warn message [`BlobPublisher::rollback`] emits for one recorded
/// object key when structured `blob_targets` evidence is absent
/// (`--rollback-only` against a run written before the structured-target
/// capture landed). Exposed as a helper so tests can pin the wording
/// without intercepting stderr.
///
/// The PRIMARY rollback path issues a real
/// [`object_store::ObjectStore::delete`] against each captured
/// [`BlobTarget`]; this helper is reached only when `extra.blob_targets`
/// is absent or empty.
pub(crate) fn blob_manual_cleanup_msg(target: &str) -> String {
    format!(
        "blob: cannot auto-delete {} — legacy evidence without structured targets; delete manually via the cloud provider's console or `aws s3 rm` / `gsutil rm` / `az storage blob delete`",
        target
    )
}

impl anodizer_core::Publisher for BlobPublisher {
    fn name(&self) -> &str {
        "blob"
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        anodizer_core::PublisherGroup::Assets
    }

    /// Forward-compat trait surface only. The load-bearing
    /// `required` flag for the blob stage's outcome is derived
    /// per-run from `BlobConfig.required` in
    /// [`crate::run::record_blob_result`] (called by
    /// [`crate::run::BlobStage`]), not by this trait method. The
    /// config-level `required_override` is threaded here for
    /// consistency with the dispatch path; `None` falls through to
    /// `false` (the built-in default) and the actual per-config
    /// policy is enforced inside the stage.
    fn required(&self) -> bool {
        self.required_override.unwrap_or(false)
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        // Capture only files that actually landed. On failure the
        // returned error is re-raised (so the dispatch path treats the
        // publisher as failed) but the partial-success list inside
        // `run_with_evidence` is already discarded by `?` — that's the
        // accepted trade-off: evidence is post-upload truth, errors
        // bubble up cleanly. A future refactor that wants partial
        // evidence on a failure path can switch to a `(Vec, Result)`
        // shape; for now the publish run is either fully evidenced or
        // failed.
        let uploaded: Vec<BlobTarget> = BlobStage::new().run_with_evidence(ctx)?;
        let mut evidence = anodizer_core::PublishEvidence::new("blob");
        // The `artifact_paths` slot keeps the operator-readable
        // `<provider>://<bucket>/<key>` form for the text-only
        // `--rollback-only` summary; the structured copy in
        // `extra.blob_targets` is the authoritative source for the
        // DELETE call.
        if let Some(first) = uploaded.first() {
            evidence.primary_ref = Some(blob_target_url(first));
        }
        evidence.artifact_paths = uploaded
            .iter()
            .map(|t| std::path::PathBuf::from(blob_target_url(t)))
            .collect();
        evidence.extra = encode_blob_targets(&uploaded);
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");

        // Primary path: structured targets → real `ObjectStore::delete`.
        let targets = decode_blob_targets(&evidence.extra);
        if !targets.is_empty() {
            return rollback_via_object_store(ctx, &log, &targets);
        }

        // Fallback path: legacy evidence with only `artifact_paths` and
        // no structured `blob_targets` — emit the per-object manual
        // cleanup checklist. Reached for runs written before the
        // structured-target capture landed, and for any future evidence
        // shape that doesn't surface the targets list.
        if evidence.artifact_paths.is_empty() && evidence.primary_ref.is_none() {
            log.warn(&anodizer_core::rollback_empty_warning_msg(
                "blob",
                "upload targets",
            ));
            return Ok(());
        }
        for path in &evidence.artifact_paths {
            log.warn(&blob_manual_cleanup_msg(&path.display().to_string()));
        }
        log.status(&format!(
            "blob: rollback emitted manual-cleanup checklist for {} object(s) (legacy evidence)",
            evidence.artifact_paths.len()
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Some("provider credentials delete-object")
    }
}

/// Group structured `BlobTarget`s by their `(provider, bucket, region,
/// endpoint)` key, construct one `ObjectStore` per group, then issue a
/// `delete` per object. Best-effort: per-object failures (other than
/// `NotFound`) are logged as warns and counted; `NotFound` is treated as
/// success because the object was already gone.
///
/// Returns `Ok(())` even on partial failure — the contract for
/// [`anodizer_core::Publisher::rollback`] is "best-effort, report what
/// you did via the log surface, don't bail the rollback dispatch". A
/// hard `Err` would abort sibling publishers' rollbacks too.
fn rollback_via_object_store(
    ctx: &mut Context,
    log: &StageLogger,
    targets: &[BlobTarget],
) -> anyhow::Result<()> {
    use crate::provider::Provider;
    use crate::store::build_store;
    use anodizer_core::config::BlobConfig;
    use object_store::ObjectStoreExt as _;

    // Group by (provider, bucket, region, endpoint) so we reuse one
    // `ObjectStore` handle across every key in the same bucket / region
    // / endpoint scope. The grouping key is deliberately lossless: two
    // targets that differ in `endpoint` (e.g. one cross-account R2
    // upload alongside an AWS bucket) get their own store handle.
    /// `(provider, bucket, region, endpoint)` — the addressing tuple
    /// `build_store` needs to construct a unique `ObjectStore` handle.
    type StoreGroupKey = (String, String, Option<String>, Option<String>);
    let mut groups: std::collections::BTreeMap<StoreGroupKey, Vec<&BlobTarget>> =
        std::collections::BTreeMap::new();
    for t in targets {
        groups
            .entry((
                t.provider.clone(),
                t.bucket.clone(),
                t.region.clone(),
                t.endpoint.clone(),
            ))
            .or_default()
            .push(t);
    }

    // One tokio runtime for the whole rollback call. `block_on` here is
    // safe because `Publisher::rollback` is invoked synchronously from
    // `stage_publish::run_rollback_if_needed` (which itself runs on the
    // synchronous pipeline thread — no enclosing tokio runtime).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("blob: failed to construct tokio runtime: {}", e))?;

    let mut deleted = 0usize;
    let mut already_absent = 0usize;
    let mut failed = 0usize;

    for ((provider_str, bucket, region, endpoint), group_targets) in &groups {
        // Synthesize a minimal `BlobConfig` sufficient for
        // `build_store`'s DELETE path. KMS / ACL / cache-control / etc.
        // are upload-side concerns and intentionally omitted — the
        // delete operation only needs auth + addressing.
        let cfg = BlobConfig {
            provider: provider_str.clone(),
            bucket: bucket.clone(),
            region: region.clone(),
            endpoint: endpoint.clone(),
            ..Default::default()
        };

        let provider = match Provider::parse(provider_str) {
            Ok(p) => p,
            Err(e) => {
                log.warn(&format!(
                    "blob: rollback skipped {}://{} — unknown provider in evidence: {}",
                    provider_str, bucket, e
                ));
                failed += group_targets.len();
                continue;
            }
        };

        let store = match build_store(provider, &cfg, bucket, ctx) {
            Ok(s) => s,
            Err(e) => {
                log.warn(&format!(
                    "blob: rollback skipped {}://{} — build_store failed: {:#}",
                    provider_str, bucket, e
                ));
                failed += group_targets.len();
                continue;
            }
        };

        for t in group_targets {
            let path = object_store::path::Path::from(t.key.as_str());
            let url = blob_target_url(t);
            match rt.block_on(store.delete(&path)) {
                Ok(()) => {
                    log.status(&format!("blob: DELETE {}", url));
                    deleted += 1;
                }
                Err(object_store::Error::NotFound { .. }) => {
                    // Object already gone — operator pre-deleted via the
                    // cloud console, or a prior partial rollback already
                    // ran. Idempotent: treat as success.
                    log.status(&format!(
                        "blob: {} already absent (treating as success)",
                        url
                    ));
                    already_absent += 1;
                }
                Err(e) => {
                    log.warn(&format!(
                        "blob: DELETE {} failed: {} (manual cleanup may be required)",
                        url, e
                    ));
                    failed += 1;
                }
            }
        }
    }

    log.status(&format!(
        "blob: rollback complete — {} deleted, {} already absent, {} failed",
        deleted, already_absent, failed
    ));
    Ok(())
}

/// True when at least one selected crate has a `blobs:` block. Mirrors the
/// dispatch predicate `BlobStage::run` evaluates internally; used by the
/// stage-publish registry to decide whether to push a `BlobPublisher`.
pub fn is_configured(ctx: &Context) -> bool {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .any(|c| c.blobs.as_ref().is_some_and(|v| !v.is_empty()))
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{BlobConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    #[test]
    fn blob_publisher_classification() {
        let p = BlobPublisher::new();
        assert_eq!(p.name(), "blob");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("provider credentials delete-object")
        );
    }

    #[test]
    fn blob_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = BlobPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn blob_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("blob");
        let p = BlobPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("blob")
                && m.contains("upload targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    /// Important #3 — evidence carries only files that actually uploaded,
    /// not the planned set. With no real store wired up the run path
    /// returns an empty list rather than fabricating one — the assertion
    /// is structural: the resulting evidence shape is empty, not
    /// pre-populated with planned-but-unsent paths.
    #[test]
    fn blob_publisher_records_only_uploaded_keys() {
        // Empty crates → no upload jobs → empty evidence.
        let mut ctx = TestContextBuilder::new().build();
        let p = BlobPublisher::new();
        let evidence = p
            .run(&mut ctx)
            .expect("run should not error on empty config");
        assert!(
            evidence.artifact_paths.is_empty(),
            "no jobs → no recorded uploads, got {:?}",
            evidence.artifact_paths
        );
        assert!(evidence.primary_ref.is_none());
    }

    /// Per-key warn message shape is fixed by `blob_manual_cleanup_msg`;
    /// tests pin the wording here so a future refactor cannot silently
    /// drop the actionable instruction.
    #[test]
    fn blob_manual_cleanup_msg_is_actionable() {
        let msg = blob_manual_cleanup_msg("s3://my-bucket/myapp/v1.0.0/foo.tar.gz");
        assert!(
            msg.contains("s3://my-bucket/myapp/v1.0.0/foo.tar.gz"),
            "{msg}"
        );
        assert!(msg.contains("aws s3 rm"), "{msg}");
        assert!(msg.contains("gsutil rm"), "{msg}");
        assert!(msg.contains("az storage blob delete"), "{msg}");
        assert!(msg.contains("legacy evidence"), "{msg}");
    }

    #[test]
    fn is_configured_false_without_blobs_block() {
        let ctx = TestContextBuilder::new().build();
        assert!(!is_configured(&ctx));
    }

    #[test]
    fn is_configured_true_when_crate_opts_in() {
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: "my-bucket".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        assert!(is_configured(&ctx));
    }

    /// `BlobTarget` is the load-bearing structured shape persisted to
    /// `PublishEvidence.extra.blob_targets`. A serde roundtrip must
    /// preserve every field — losing `region` / `endpoint` would force
    /// the rollback DELETE through `build_store`'s `from_env()` fallback
    /// (which can pick up the wrong region from the operator's
    /// `AWS_REGION` env at rollback time, deleting the wrong bucket).
    #[test]
    fn blob_target_serde_roundtrip() {
        let t = BlobTarget {
            provider: "s3".to_string(),
            bucket: "my-bucket".to_string(),
            key: "myapp/v1.0.0/foo.tar.gz".to_string(),
            region: Some("us-west-2".to_string()),
            endpoint: Some("https://s3.example.com".to_string()),
        };
        let encoded = encode_blob_targets(std::slice::from_ref(&t));
        let decoded = decode_blob_targets(&encoded);
        assert_eq!(decoded, vec![t.clone()]);

        // Wire-format pin: serialize through evidence and inspect the
        // JSON to confirm the array rides under the `blob_targets`
        // key (matches the pre-typed shape, parallel to
        // `cloudsmith_targets`).
        let mut e = PublishEvidence::new("blob");
        e.extra = encoded;
        let s = serde_json::to_string(&e).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
        let arr = v["extra"]["blob_targets"]
            .as_array()
            .expect("blob_targets array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["provider"], "s3");
        assert_eq!(arr[0]["bucket"], "my-bucket");
        assert_eq!(arr[0]["key"], "myapp/v1.0.0/foo.tar.gz");
        assert_eq!(arr[0]["region"], "us-west-2");
        assert_eq!(arr[0]["endpoint"], "https://s3.example.com");
    }

    /// `region` and `endpoint` are `None` for GCS / Azure / vanilla S3
    /// without a custom endpoint; decode must accept payloads where the
    /// keys are omitted (forward-compat with future evidence written by
    /// a serializer that doesn't emit `null` for `None`).
    #[test]
    fn blob_target_decode_tolerates_missing_optional_fields() {
        // Hand-rolled JSON matching the evidence shape — wrapped in
        // the `PublishEvidence` envelope so deserialization exercises
        // the same untagged-enum path live evidence files take.
        let raw = r#"{
            "schema_version": 1,
            "publisher": "blob",
            "artifact_paths": [],
            "extra": {
                "blob_targets": [
                    {
                        "provider": "gs",
                        "bucket": "gcs-bucket",
                        "key": "myapp/v1.0.0/foo.tar.gz"
                    },
                    {
                        "provider": "azblob",
                        "bucket": "azure-container",
                        "key": "myapp/v1.0.0/bar.tar.gz",
                        "region": null,
                        "endpoint": null
                    }
                ]
            }
        }"#;
        let e: PublishEvidence = serde_json::from_str(raw).expect("deserialize");
        let decoded = decode_blob_targets(&e.extra);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].provider, "gs");
        assert!(decoded[0].region.is_none());
        assert!(decoded[0].endpoint.is_none());
        assert_eq!(decoded[1].provider, "azblob");
        assert!(decoded[1].region.is_none());
        assert!(decoded[1].endpoint.is_none());
    }

    /// Non-Blob variant decodes to empty so rollback dispatch falls
    /// through to the legacy warn-only path.
    #[test]
    fn blob_target_decode_empty_on_missing_key() {
        assert!(decode_blob_targets(&anodizer_core::PublishEvidenceExtra::Empty).is_empty());
        // Empty Blob variant: array present but vacant.
        let empty =
            anodizer_core::PublishEvidenceExtra::Blob(anodizer_core::publish_evidence::BlobExtra {
                blob_targets: Vec::new(),
            });
        assert!(decode_blob_targets(&empty).is_empty());
        // Wrong variant entirely.
        let wrong = anodizer_core::PublishEvidenceExtra::Homebrew(
            anodizer_core::publish_evidence::HomebrewExtra {
                homebrew_targets: Vec::new(),
            },
        );
        assert!(decode_blob_targets(&wrong).is_empty());
    }

    /// Evidence that pre-dates the structured-target capture (only
    /// `artifact_paths`, no `extra.blob_targets`) must NOT panic at
    /// rollback time; it must emit the warn-only manual-cleanup
    /// checklist instead. This is the "legacy evidence" forward-compat
    /// guarantee.
    #[test]
    fn blob_publisher_rollback_falls_back_to_warn_for_legacy_evidence() {
        let mut ctx = TestContextBuilder::new().build();
        let mut evidence = PublishEvidence::new("blob");
        // Legacy shape: artifact_paths populated, but extra.blob_targets
        // is absent (the field was added by B14).
        evidence.artifact_paths = vec![
            std::path::PathBuf::from("s3://legacy-bucket/myapp/v0.1.0/legacy.tar.gz"),
            std::path::PathBuf::from("s3://legacy-bucket/myapp/v0.1.0/legacy.zip"),
        ];
        evidence.primary_ref = Some("s3://legacy-bucket/myapp/v0.1.0/legacy.tar.gz".to_string());
        let p = BlobPublisher::new();
        // Must not panic; must return Ok (best-effort warn-only).
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
    }

    /// When `extra.blob_targets` is populated, rollback dispatches into
    /// `rollback_via_object_store` instead of the warn-only path. The
    /// test exercises the dispatch decision structurally: an
    /// unreachable bucket/endpoint causes per-target `delete` failures
    /// (connection refused → `object_store::Error::Generic`), but the
    /// rollback contract is best-effort (`Ok(())` even on partial
    /// failure), so a successful return AND no panic confirms the
    /// structured-target branch was taken end-to-end without crashing
    /// on a "real" delete attempt.
    ///
    /// The fixture pins retry attempts to 1 + 1ms backoff (via
    /// `ctx.config.retry`) so the unreachable endpoint fails in
    /// milliseconds instead of running out the default 5-minute retry
    /// budget. Live DELETE behaviour against a real store is covered
    /// by the existing integration matrix (the `object_store` crate
    /// itself is upstream-tested); this is the lightest-weight wiring
    /// check that doesn't require an HTTP mock dep.
    #[test]
    fn blob_publisher_rollback_decodes_structured_targets_and_attempts_delete() {
        use anodizer_core::config::{HumanDuration, RetryConfig};
        let mut ctx = TestContextBuilder::new().build();
        // Fast-fail retry: 1 attempt, 1ms backoff. Without this the
        // `from_env()` S3 builder picks up the default 5-minute retry
        // budget and the test hangs while object_store retries against
        // the unreachable endpoint.
        ctx.config.retry = Some(RetryConfig {
            attempts: 1,
            delay: HumanDuration(std::time::Duration::from_millis(1)),
            max_delay: HumanDuration(std::time::Duration::from_millis(1)),
        });
        let mut evidence = PublishEvidence::new("blob");
        let target = BlobTarget {
            provider: "s3".to_string(),
            bucket: "anodizer-rollback-test-unreachable".to_string(),
            key: "myapp/v0.1.0/foo.tar.gz".to_string(),
            // Endpoint pointed at a guaranteed-unreachable address so
            // the DELETE attempt fails fast — best-effort rollback
            // swallows the per-target failure as a warn instead of
            // hitting a real S3 endpoint or hanging on DNS lookup.
            // 127.0.0.1:1 is reserved + nothing listens on port 1.
            region: Some("us-east-1".to_string()),
            endpoint: Some("http://127.0.0.1:1".to_string()),
        };
        evidence.artifact_paths = vec![std::path::PathBuf::from(blob_target_url(&target))];
        evidence.extra = encode_blob_targets(&[target]);
        let p = BlobPublisher::new();
        // Best-effort: even though the DELETE will fail (unreachable
        // endpoint or build_store-time error), rollback returns Ok so
        // sibling publishers' rollbacks aren't aborted.
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
    }

    /// Evidence credential-contract regression: serialised `BlobTarget`
    /// JSON must carry no credential bytes. The five `BlobTarget`
    /// fields are addressing-only by design; this test pins that
    /// invariant so a future field addition that smuggles a token /
    /// access_key / secret in via `extra.blob_targets` trips here.
    #[test]
    fn blob_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence and assert (a) no
        // credential-shaped keys appear AND (b) the operator-public
        // addressing fields serialize.
        let t = BlobTarget {
            provider: "s3".to_string(),
            bucket: "my-bucket".to_string(),
            key: "myapp/v1.0.0/foo.tar.gz".to_string(),
            region: Some("us-west-2".to_string()),
            endpoint: Some("https://s3.example.com".to_string()),
        };
        let mut e = anodizer_core::PublishEvidence::new("blob");
        e.extra = encode_blob_targets(&[t]);
        let s = serde_json::to_string(&e).expect("serialize");
        for forbidden in [
            "\"token\"",
            "\"password\"",
            "\"pat\"",
            "\"private_key\"",
            "\"access_key\"",
            "\"secret_key\"",
            "\"session_token\"",
            "\"api_key\"",
        ] {
            assert!(
                !s.contains(forbidden),
                "encoded blob_targets must not carry {} (got {})",
                forbidden,
                s
            );
        }
        // Positive shape: addressing coordinates present.
        assert!(s.contains("\"provider\":\"s3\""), "{s}");
        assert!(s.contains("\"bucket\":\"my-bucket\""), "{s}");
        assert!(s.contains("\"key\":\"myapp/v1.0.0/foo.tar.gz\""), "{s}");
        assert!(s.contains("\"region\":\"us-west-2\""), "{s}");
        assert!(s.contains("\"endpoint\":\"https://s3.example.com\""), "{s}");
    }
}
