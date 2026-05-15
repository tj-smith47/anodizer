use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anodizer_core::{PublishReport, PublisherGroup, PublisherOutcome, PublisherResult};

use object_store::{ObjectStore, PutOptions};

use crate::kms::{KmsProvider, parse_kms_provider, preflight_kms_cli, validate_kms_provider_match};
use crate::provider::Provider;
use crate::store::build_store;
use crate::upload::{
    build_put_options, collect_artifacts, format_remote_path, resolve_extra_files,
    upload_files_owned,
};

// ---------------------------------------------------------------------------
// validate_only — config-validation entry point, no I/O dispatched
// ---------------------------------------------------------------------------

/// Render the provider template, parse it through [`Provider::parse`], and
/// build the corresponding `ObjectStore` so any provider-keyed validators
/// (S3 canned-ACL gate, GCS predefined-ACL gate, KMS provider/scheme match,
/// …) execute. The store itself is discarded — no network is dispatched.
///
/// Used by Q9.1 regression tests to pin the contract that a templated
/// `provider:` field flows through the same dispatch as a literal one. The
/// surface is `pub(crate)` (test-only consumption); production callers
/// should use [`BlobStage::run`].
#[cfg(test)]
pub(crate) fn validate_only(
    blob_cfg: &anodizer_core::config::BlobConfig,
    ctx: &Context,
) -> Result<()> {
    if blob_cfg.provider.is_empty() {
        anyhow::bail!("blobs: provider is required");
    }
    if blob_cfg.bucket.is_empty() {
        anyhow::bail!("blobs: bucket is required");
    }

    let provider_str = ctx
        .render_template(&blob_cfg.provider)
        .with_context(|| format!("blobs: render provider template '{}'", blob_cfg.provider))?;
    let provider = Provider::parse(&provider_str)?;

    let rendered_bucket = ctx
        .render_template(&blob_cfg.bucket)
        .with_context(|| format!("blobs: render bucket template '{}'", blob_cfg.bucket))?;

    // build_store fans out to build_s3_store / build_gcs_store / ... — the
    // provider-keyed validator chain (canned ACLs, KMS scheme match, …)
    // runs here. We only need the side-effect of validation; the store is
    // dropped immediately.
    let _store = build_store(provider, blob_cfg, &rendered_bucket, ctx)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// BlobStage
// ---------------------------------------------------------------------------

pub struct BlobStage;

impl BlobStage {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BlobStage {
    fn default() -> Self {
        Self::new()
    }
}

/// A fully-prepared blob upload job. Phase 1 (serial, `&mut ctx`) renders
/// templates, builds the ObjectStore, pre-renders per-item put options;
/// Phase 2 (parallel) runs the per-config upload via `upload_files_owned`.
/// Workers never touch `ctx`.
struct BlobJob {
    provider_display: &'static str,
    rendered_bucket: String,
    rendered_directory: String,
    upload_items: Vec<(PathBuf, String)>,
    store: Arc<dyn ObjectStore>,
    put_opts_per_item: Vec<PutOptions>,
    parallelism_inner: usize,
    client_kms: Option<(String, KmsProvider)>,
}

impl Stage for BlobStage {
    fn name(&self) -> &str {
        "blob"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        // BlobStage is Assets-group and writes its own outcome into
        // `ctx.publish_report` so the SnapcraftPublishStage submitter
        // gate (which runs AFTER BlobStage per the pipeline order) sees
        // a required-blob failure via the standard `any_failed(Assets,
        // required_only=true)` path. The publisher dispatch in
        // `PublishStage::run` runs BEFORE BlobStage so `ctx.publish_report`
        // is already initialized in the common case; the `None` branch
        // covers `--publish` subset runs where the publish stage was
        // skipped.
        //
        // Per-target failure becomes `PublisherOutcome::Failed(_)` + Ok(()).
        // Catastrophic errors (missing required config, malformed
        // provider, IO impossible at the stage boundary) still bubble up
        // via the `?` operator in `run_with_evidence` -> `prepare_jobs`.
        let (uploaded, exec_result) = self.run_report(ctx)?;
        if let Some(exec_result) = exec_result {
            // Aggregated `required` across every blob config on every
            // selected crate: when ANY blob config opts in
            // (`required: true`), the recorded outcome carries
            // `required = true` so a failed upload trips the submitter
            // gate (`any_failed(Assets, required_only=true)`) and the
            // CLI's required-failures exit-code gate. See
            // `BlobConfig.required` rustdoc for the semantics.
            let derived_required = derive_blob_required(ctx);
            record_blob_result(ctx, &uploaded, &exec_result, derived_required);
        }
        // Per-target upload errors are reported via PublisherResult;
        // they must NOT bail the pipeline because the same gate that
        // protects irreversible Submitter publishers depends on
        // post-blob stages still running (e.g. announce-gating).
        Ok(())
    }
}

/// Append a `PublisherResult` for the blob stage to `ctx.publish_report`.
/// Initializes the report when `None` (covers `--publish` runs where
/// `PublishStage` was skipped). The `required` flag is derived from
/// `BlobConfig.required` — when any blob config is required, the
/// recorded outcome carries `required = true` so the submitter gate
/// and the CLI's required-failures exit-code gate fire on a failed
/// blob upload.
pub(crate) fn record_blob_result(
    ctx: &mut Context,
    uploaded: &[String],
    exec_result: &Result<()>,
    required: bool,
) {
    let outcome = match exec_result {
        Ok(()) => PublisherOutcome::Succeeded,
        Err(e) => PublisherOutcome::Failed(format!("{e:#}")),
    };
    let evidence = match exec_result {
        Ok(()) if !uploaded.is_empty() => {
            let mut e = anodizer_core::PublishEvidence::new("blob");
            e.primary_ref = Some(uploaded[0].clone());
            e.artifact_paths = uploaded.iter().map(std::path::PathBuf::from).collect();
            Some(e)
        }
        _ => None,
    };
    if ctx.publish_report.is_none() {
        ctx.publish_report = Some(PublishReport::default());
    }
    let report = ctx
        .publish_report
        .as_mut()
        .expect("publish_report initialized above");
    report.results.push(PublisherResult {
        name: "blob".to_string(),
        group: PublisherGroup::Assets,
        required,
        outcome,
        evidence,
    });
}

/// Derive the aggregated `required` flag for the blob stage's
/// `PublisherResult`: `true` iff any selected crate's blob config sets
/// `required: true`. Keeps semantics simple — one aggregated outcome
/// per stage, one bit per stage — so the submitter gate just consults
/// `any_failed(Assets, required_only=true)` without per-config
/// bookkeeping.
pub(crate) fn derive_blob_required(ctx: &Context) -> bool {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter_map(|c| c.blobs.as_ref())
        .flat_map(|configs| configs.iter())
        .any(|cfg| cfg.required.unwrap_or(false))
}

impl BlobStage {
    /// Execute the blob upload like [`Stage::run`] but return the list of
    /// `<provider>://<bucket>/<key>` URLs that were actually uploaded.
    /// `BlobPublisher::run` calls this so `PublishEvidence::artifact_paths`
    /// reflects only files that landed — the prior pre-upload capture
    /// produced a rollback checklist that referenced files which never
    /// existed when a mid-stream upload failed.
    ///
    /// On error: returns the list of files that succeeded before the
    /// failure (via [`anyhow::Error::downcast`] handoff), so the caller
    /// can still emit a partial rollback checklist. The current
    /// implementation runs the upload phase atomically per job; partial
    /// success is captured up to the failing job's boundary.
    pub fn run_with_evidence(&self, ctx: &mut Context) -> Result<Vec<String>> {
        let (keys, exec) = self.run_report(ctx)?;
        if let Some(r) = exec {
            r?;
        }
        Ok(keys)
    }

    /// Like [`Self::run_with_evidence`] but splits the catastrophic
    /// pre-flight / setup errors (returned as the outer `Result::Err`,
    /// matching the public `run_with_evidence` contract) from the
    /// upload-phase outcome.
    ///
    /// Return shape:
    /// - `Err(_)`: catastrophic — config validation failed, runtime
    ///   construction failed, etc. Bubbled out of `Stage::run` as the
    ///   pipeline-failing error.
    /// - `Ok((keys, None))`: no work was attempted (snapshot-skip, no
    ///   configured crates, every job was disabled or had no files).
    ///   `Stage::run` does NOT append a `PublisherResult` in this case.
    /// - `Ok((keys, Some(Ok(()))))`: at least one job ran and every
    ///   upload succeeded.
    /// - `Ok((keys, Some(Err(_))))`: at least one job ran and at least
    ///   one upload failed; `keys` carries the partial-success list
    ///   captured up to the failure.
    ///
    /// `Stage::run` consumes the `Option<Result<()>>` to decide whether
    /// to record a `PublisherOutcome::Succeeded` / `Failed(_)` entry.
    fn run_report(&self, ctx: &mut Context) -> Result<(Vec<String>, Option<Result<()>>)> {
        let log = ctx.logger("blob");
        if ctx.skip_in_snapshot(&log, "blob") {
            return Ok((Vec::new(), None));
        }

        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let global_parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have blob config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.blobs.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok((Vec::new(), None));
        }

        // Pre-flight: when `provider` is a literal (no template syntax),
        // validate it via Provider::parse before any I/O so a typo (e.g.
        // `provider: gss`) fails immediately instead of after build/archive.
        // Template-bearing providers (`{{ ... }}`) are validated inside the
        // per-job loop after rendering. Provider::parse already mentions the
        // bad value and the valid set, so the error needs no extra context.
        for krate in &crates {
            if let Some(blob_configs) = krate.blobs.as_ref() {
                for blob_cfg in blob_configs {
                    if !blob_cfg.provider.is_empty() && !blob_cfg.provider.contains("{{") {
                        Provider::parse(&blob_cfg.provider)?;
                    }
                }
            }
        }

        // Phase 1 (serial): render every config, build stores, collect jobs.
        let mut jobs: Vec<BlobJob> = Vec::new();

        for krate in &crates {
            // SAFETY: `crates` was filtered to only include crates with
            // `blobs.is_some()` above, so this Option is always Some here.
            // `continue` defends against a future refactor that breaks the
            // invariant rather than panicking on the now-impossible None.
            let Some(blob_configs) = krate.blobs.as_ref() else {
                continue;
            };

            for blob_cfg in blob_configs {
                // Evaluate disable (supports both bool and template string)
                if ctx.skip_with_log(
                    &blob_cfg.skip,
                    &log,
                    &format!("blob config for crate {}", krate.name),
                )? {
                    continue;
                }

                // Validate required fields
                if blob_cfg.provider.is_empty() {
                    anyhow::bail!("blobs: provider is required for crate '{}'", krate.name);
                }
                if blob_cfg.bucket.is_empty() {
                    anyhow::bail!("blobs: bucket is required for crate '{}'", krate.name);
                }

                let provider_str = ctx.render_template(&blob_cfg.provider).with_context(|| {
                    format!(
                        "blobs: render provider template '{}' for crate '{}'",
                        blob_cfg.provider, krate.name
                    )
                })?;
                let provider = Provider::parse(&provider_str)?;
                let config_label = blob_cfg.id.as_deref().unwrap_or(&provider_str);

                // Render template fields
                let rendered_bucket = ctx.render_template(&blob_cfg.bucket).with_context(|| {
                    format!(
                        "blobs[{}]: render bucket template for crate {}",
                        config_label, krate.name
                    )
                })?;

                // Default mirrors GoReleaser's `{{ .ProjectName }}/{{ .Tag }}`
                // (blob.go:27) but expressed in Tera syntax (no leading `.`).
                // Anodizer's renderer accepts both forms, so a YAML lifted from
                // a goreleaser config that overrides `directory:` keeps working.
                let directory_template = blob_cfg
                    .directory
                    .as_deref()
                    .unwrap_or("{{ ProjectName }}/{{ Tag }}");
                let rendered_directory =
                    ctx.render_template(directory_template).with_context(|| {
                        format!(
                            "blobs[{}]: render directory template for crate {}",
                            config_label, krate.name
                        )
                    })?;

                log.status(&format!(
                    "uploading to {} {}/{}",
                    provider.display_name(),
                    rendered_bucket,
                    rendered_directory
                ));

                // Collect artifacts to upload
                let mut upload_items: Vec<(PathBuf, String)> = Vec::new();

                let artifacts = collect_artifacts(ctx, blob_cfg, &krate.name);
                for artifact in &artifacts {
                    let filename = artifact
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("artifact")
                        .to_string();
                    upload_items.push((artifact.path.clone(), filename));
                }

                // Resolve extra files (with template-rendered names)
                if let Some(ref extra_files) = blob_cfg.extra_files {
                    let resolved = resolve_extra_files(extra_files, ctx, &log)?;
                    upload_items.extend(resolved);
                }

                // Process templated_extra_files: render and add to upload list.
                // NOTE: Rendered files are written to the shared dist directory. If multiple
                // blob configs use the same dst name, later writes will overwrite earlier
                // ones. Users should ensure dst names are unique across configs.
                if let Some(ref tpl_specs) = blob_cfg.templated_extra_files
                    && !tpl_specs.is_empty()
                {
                    let rendered = anodizer_core::templated_files::process_templated_extra_files(
                        tpl_specs,
                        ctx,
                        &ctx.config.dist,
                        "blobs",
                    )?;
                    upload_items.extend(rendered);
                }

                // Note: metadata files are already handled by collect_artifacts()
                // when include_meta is true — it includes ArtifactKind::Metadata
                // in its filter. No separate scan needed here.

                if upload_items.is_empty() {
                    log.warn(&format!(
                        "no files to upload for blob config on crate '{}'",
                        krate.name
                    ));
                    continue;
                }

                if dry_run {
                    // Dry-run: log what would happen without constructing the store
                    for (local_path, remote_key) in &upload_items {
                        let remote = format_remote_path(
                            provider,
                            &rendered_bucket,
                            &rendered_directory,
                            remote_key,
                        );
                        log.status(&format!(
                            "[dry-run] would upload {} -> {}",
                            local_path.display(),
                            remote,
                        ));
                    }
                    continue;
                }

                // Log each file before upload (serial stays in Phase 1 so
                // the per-config announcement order remains deterministic,
                // matching the pre-parallel behaviour).
                for (local_path, remote_key) in &upload_items {
                    let remote = format_remote_path(
                        provider,
                        &rendered_bucket,
                        &rendered_directory,
                        remote_key,
                    );
                    log.status(&format!("uploading {} -> {}", local_path.display(), remote));
                }

                let store: Arc<dyn ObjectStore> =
                    Arc::from(build_store(provider, blob_cfg, &rendered_bucket, ctx)?);

                // Pre-render put options per item while we still hold &ctx.
                let put_opts_per_item: Vec<PutOptions> = upload_items
                    .iter()
                    .map(|(_, key)| build_put_options(blob_cfg, key, ctx))
                    .collect::<Result<_>>()?;

                // Determine if client-side KMS encryption is needed.
                // Validate KMS scheme matches the bucket provider so a misconfig
                // surfaces here, not deep inside the upload phase. Preflight
                // the CLI tool too — a missing `aws`/`gcloud`/`az` binary on
                // PATH used to fail per-artifact during fan-out, producing N
                // identical errors. One check, one error.
                let client_kms = if let Some(key) = blob_cfg.kms_key.as_deref() {
                    let kms_provider = parse_kms_provider(key);
                    validate_kms_provider_match(provider, kms_provider, key)?;
                    preflight_kms_cli(kms_provider)?;
                    match kms_provider {
                        KmsProvider::ServerSide => None,
                        _ => Some((key.to_string(), kms_provider)),
                    }
                } else {
                    None
                };

                let parallelism_inner = blob_cfg
                    .parallelism
                    .unwrap_or(ctx.options.parallelism)
                    .max(1);

                jobs.push(BlobJob {
                    provider_display: provider.display_name(),
                    rendered_bucket,
                    rendered_directory,
                    upload_items,
                    store,
                    put_opts_per_item,
                    parallelism_inner,
                    client_kms,
                });
            }
        }

        if jobs.is_empty() {
            return Ok((Vec::new(), None));
        }

        // Phase 2 (parallel across configs): each worker runs its own
        // upload loop (which itself has intra-config per-file concurrency
        // via tokio). Bounded by the global parallelism so we don't fan
        // out unbounded across both axes simultaneously.
        //
        // One tokio runtime is shared across every job — N parallel jobs
        // would otherwise allocate N independent thread pools.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("anodizer-blob")
            .build()
            .context("blob: failed to construct tokio runtime")?;
        let runtime_ref = &runtime;
        // Shared accumulator of uploaded `<scheme>://<bucket>/<key>` URLs
        // across every job. `upload_files_owned` records each successful
        // upload on its own task; the per-job wrapper translates the
        // returned object keys into provider-qualified URLs before
        // appending to the shared list. On failure the partial list is
        // preserved so PublishEvidence captures only files that landed.
        let uploaded_urls: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let run_job = |job: &BlobJob| -> Result<()> {
            match upload_files_owned(
                runtime_ref,
                Arc::clone(&job.store),
                job.upload_items.clone(),
                job.rendered_directory.clone(),
                job.put_opts_per_item.clone(),
                job.parallelism_inner,
                job.client_kms.clone(),
            ) {
                Ok(keys) => {
                    let mut acc = uploaded_urls.lock().expect("uploaded list lock");
                    for key in keys {
                        acc.push(format!(
                            "{}://{}/{}",
                            job.provider_display, job.rendered_bucket, key
                        ));
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };

        let result = anodizer_core::parallel::run_parallel_chunks(
            &jobs,
            global_parallelism,
            "blob",
            run_job,
        );
        // Snapshot the uploaded list whether the run succeeded or failed
        // so callers can record partial success in PublishEvidence.
        let mut keys = uploaded_urls.lock().expect("uploaded list lock").clone();
        keys.sort();

        if result.is_ok() {
            for job in &jobs {
                log.status(&format!(
                    "uploaded {} file(s) to {} {}/{}",
                    job.upload_items.len(),
                    job.provider_display,
                    job.rendered_bucket,
                    job.rendered_directory,
                ));
            }
        }

        // Collapse `Vec<()>` -> `()` so the inner Result has the same
        // shape as `Stage::run`'s return — the per-job successes have
        // already been folded into the shared `uploaded_urls`
        // accumulator above.
        let exec = result.map(|_| ());
        Ok((keys, Some(exec)))
    }
}
