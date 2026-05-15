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
//! Rollback shape (**deferred-with-warn**): each provider's
//! [`object_store::ObjectStore`] exposes a `delete()` API, but the
//! rollback path needs to reconstruct the store (provider, bucket, region,
//! KMS settings, credentials) from the same [`anodizer_core::config::BlobConfig`]
//! `BlobStage::run` consumed at publish time. The current
//! [`anodizer_core::PublishEvidence`] schema carries `artifact_paths` as a
//! flat `Vec<PathBuf>` — sufficient to record per-object provider URLs
//! (e.g. `s3://bucket/key`) but not sufficient to reconstruct the auth
//! context required to issue the DELETE. Until a follow-up threads a
//! richer evidence shape through the publisher pipeline, the rollback
//! emits one warn per recorded object key so an operator running
//! `--rollback-only` has a complete manual-cleanup checklist.
//!
//! Migrating to actual DELETE only requires (a) capturing the BlobConfig
//! reference alongside the object key list in evidence and (b) calling
//! `build_store(...)` from `store.rs` plus `store.delete(&path).await` in
//! the rollback. The runtime cost is one tokio block_on per object —
//! same shape as the upload phase.

use anodizer_core::context::Context;

use crate::run::BlobStage;

/// [`anodizer_core::Publisher`] adapter over [`BlobStage::run`].
///
/// Evidence records ONLY files that actually landed in the store (via
/// [`BlobStage::run_with_evidence`]). The prior pre-upload capture would
/// have given an operator running `--rollback-only` a checklist of paths
/// that never existed when a mid-stream upload failed; the post-upload
/// snapshot is the safer end state — fewer rollback items, no phantom
/// targets.
pub struct BlobPublisher;

impl BlobPublisher {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BlobPublisher {
    fn default() -> Self {
        Self::new()
    }
}

/// The exact warn message [`BlobPublisher::rollback`] emits for one
/// recorded object key when the auto-delete path is unavailable
/// (BlobConfig not surfaced in evidence yet). Exposed as a helper so
/// tests can pin the wording without intercepting stderr.
pub(crate) fn blob_manual_cleanup_msg(target: &str) -> String {
    format!(
        "blob: cannot auto-delete {} — BlobConfig not surfaced in evidence; delete manually via the cloud provider's console or `aws s3 rm` / `gsutil rm` / `az storage blob delete`",
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
    /// trait impl has no access to the active `Context`, so it
    /// returns `false` and the actual policy is enforced inside the
    /// stage. Kept as `false` so a future refactor that drops the
    /// trait-vs-stage split doesn't silently start failing pipelines
    /// that don't opt into `required:`.
    fn required(&self) -> bool {
        false
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
        let uploaded = BlobStage::new().run_with_evidence(ctx)?;
        let mut evidence = anodizer_core::PublishEvidence::new("blob");
        if let Some(first) = uploaded.first() {
            evidence.primary_ref = Some(first.clone());
        }
        evidence.artifact_paths = uploaded.into_iter().map(std::path::PathBuf::from).collect();
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        if evidence.artifact_paths.is_empty() && evidence.primary_ref.is_none() {
            log.warn(&anodizer_core::rollback_empty_warning_msg(
                "blob",
                "upload targets",
            ));
            return Ok(());
        }
        // Deferred: `object_store::ObjectStore::delete` would do the job
        // but reconstructing the store needs the per-config BlobConfig
        // (provider, region, KMS, ACL...) which isn't surfaced in
        // PublishEvidence yet. Until evidence grows that field, emit a
        // checklist so `--rollback-only` exposes the manual-cleanup
        // surface.
        for path in &evidence.artifact_paths {
            log.warn(&blob_manual_cleanup_msg(&path.display().to_string()));
        }
        log.status(&format!(
            "blob: rollback emitted manual-cleanup checklist for {} object(s)",
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
        let mut ctx = TestContextBuilder::new().build();
        let evidence = PublishEvidence::new("blob");
        let p = BlobPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        // The warn message comes from the core shared helper so blob's
        // empty-evidence wording stays consistent with the stage-publish
        // publishers.
        let msg = anodizer_core::rollback_empty_warning_msg("blob", "upload targets");
        assert!(msg.starts_with("blob:"), "{msg}");
        assert!(msg.contains("upload targets"), "{msg}");
        assert!(msg.contains("verify"), "{msg}");
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
}
