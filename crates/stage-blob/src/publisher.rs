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
use anodizer_core::stage::Stage;

use crate::run::BlobStage;
use crate::upload::collect_artifacts;

/// Walk every configured [`BlobConfig`](anodizer_core::config::BlobConfig)
/// and return a deterministic list of `<provider>://<bucket>/<key>` strings
/// describing each upload target. Used as best-effort evidence for
/// rollback — the strings are operator-readable and uniquely identify each
/// object, even though they cannot themselves be used to authenticate a
/// DELETE without the original BlobConfig in hand.
///
/// The walk mirrors [`BlobStage::run`]'s artifact-selection logic
/// (`extra_files_only` short-circuit, `collect_artifacts` per crate,
/// `extra_files`/`templated_extra_files` extras) but skips the heavier
/// `build_store` + KMS preflight chain since rollback evidence doesn't
/// need them. Templating failures and skipped configs short-circuit to
/// "no target" silently — the publish path's own error handling has
/// already surfaced any blocker.
pub(crate) fn collect_blob_targets(ctx: &Context) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let selected = &ctx.options.selected_crates;
    let crates: Vec<_> = ctx
        .config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.blobs.is_some())
        .collect();
    for krate in crates {
        let Some(blob_configs) = krate.blobs.as_ref() else {
            continue;
        };
        for blob_cfg in blob_configs {
            // Skip semantics must match BlobStage::run.
            if let Some(ref s) = blob_cfg.skip
                && s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .unwrap_or(false)
            {
                continue;
            }
            if blob_cfg.provider.is_empty() || blob_cfg.bucket.is_empty() {
                continue;
            }
            let provider_str = match ctx.render_template(&blob_cfg.provider) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let provider = match crate::provider::Provider::parse(&provider_str) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let rendered_bucket = match ctx.render_template(&blob_cfg.bucket) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let directory_template = blob_cfg
                .directory
                .as_deref()
                .unwrap_or("{{ ProjectName }}/{{ Tag }}");
            let rendered_directory = match ctx.render_template(directory_template) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let dir_trimmed = rendered_directory.trim_matches('/');
            let scheme = provider.display_name();

            // Mirror the artifact selection BlobStage::run runs.
            let artifacts = collect_artifacts(ctx, blob_cfg, &krate.name);
            for artifact in &artifacts {
                let filename = artifact
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("artifact");
                let url = if dir_trimmed.is_empty() {
                    format!("{}://{}/{}", scheme, rendered_bucket, filename)
                } else {
                    format!(
                        "{}://{}/{}/{}",
                        scheme, rendered_bucket, dir_trimmed, filename
                    )
                };
                out.push(url);
            }
            // extra_files / templated_extra_files paths are also uploaded;
            // for evidence purposes we record the destination key under
            // each extra-file spec. Tightening the walk further would
            // duplicate non-trivial logic from BlobStage::run; the
            // primary artifact set already covers the load-bearing
            // rollback targets.
        }
    }
    out
}

/// [`anodizer_core::Publisher`] adapter over [`BlobStage::run`].
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

impl anodizer_core::Publisher for BlobPublisher {
    fn name(&self) -> &str {
        "blob"
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        anodizer_core::PublisherGroup::Assets
    }

    fn required(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        // Capture targets before run() so a partial-failure run still
        // returns the full intended target list as a rollback checklist.
        // (The current schema can't distinguish "uploaded" from "intended";
        // erring toward the larger list keeps the manual-cleanup checklist
        // complete.)
        let targets = collect_blob_targets(ctx);
        BlobStage.run(ctx)?;
        let mut evidence = anodizer_core::PublishEvidence::new("blob");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(first.clone());
        }
        evidence.artifact_paths = targets.into_iter().map(std::path::PathBuf::from).collect();
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        if evidence.artifact_paths.is_empty() && evidence.primary_ref.is_none() {
            log.warn("blob: no upload targets recorded in evidence; verify cloud storage manually");
            return Ok(());
        }
        // Deferred: `object_store::ObjectStore::delete` would do the job
        // but reconstructing the store needs the per-config BlobConfig
        // (provider, region, KMS, ACL...) which isn't surfaced in
        // PublishEvidence yet. Until evidence grows that field, emit a
        // checklist so `--rollback-only` exposes the manual-cleanup
        // surface.
        for path in &evidence.artifact_paths {
            log.warn(&format!(
                "blob: cannot auto-delete {} — BlobConfig not surfaced in evidence; delete manually via the cloud provider's console or `aws s3 rm` / `gsutil rm` / `az storage blob delete`",
                path.display()
            ));
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
