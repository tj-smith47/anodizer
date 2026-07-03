use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anodizer_core::{PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason};

use object_store::{ObjectStore, PutOptions};

use crate::kms::{KmsProvider, parse_kms_provider, preflight_kms_cli, validate_kms_provider_match};
use crate::provider::Provider;
use crate::publisher::{BlobTarget, blob_target_url};
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

/// A fully-prepared blob upload job. The serial phase (`&mut ctx`) renders
/// templates, builds the ObjectStore, and pre-renders per-item put options;
/// the parallel phase runs the per-config upload via `upload_files_owned`.
/// Workers never touch `ctx`.
struct BlobJob {
    provider_display: &'static str,
    rendered_bucket: String,
    rendered_directory: String,
    /// Rendered (post-template) S3 region, threaded through so the
    /// publisher can capture it into [`crate::publisher::BlobTarget`] for
    /// the DELETE-on-rollback path.
    rendered_region: Option<String>,
    /// Rendered (post-template) S3-compatible endpoint URL.
    rendered_endpoint: Option<String>,
    upload_items: Vec<(PathBuf, String)>,
    store: Arc<dyn ObjectStore>,
    put_opts_per_item: Vec<PutOptions>,
    parallelism_inner: usize,
    client_kms: Option<(String, KmsProvider)>,
}

/// Outcome of [`BlobStage::run_report`]: the rollback targets, the upload
/// execution result, and how many objects were skipped because an identical
/// copy already existed (drives the idempotent-skip outcome).
struct BlobRunReport {
    targets: Vec<BlobTarget>,
    exec: Option<Result<()>>,
    skipped_identical: usize,
}

impl BlobRunReport {
    /// No upload work was attempted (snapshot-skip / no configured crates /
    /// every job disabled). `Stage::run` records no `PublisherResult`.
    fn no_work() -> Self {
        Self {
            targets: Vec::new(),
            exec: None,
            skipped_identical: 0,
        }
    }
}

impl Stage for BlobStage {
    fn name(&self) -> &str {
        "blob"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        // Operator-selection gate. BlobStage performs an external,
        // irreversible object-store upload but runs as a pipeline stage
        // OUTSIDE the trait-based dispatch chokepoint, so the uniform
        // `--skip` / `--publishers` filter that governs every dispatched
        // publisher does not reach it. Consult `publisher_deselected("blob")`
        // here — BEFORE the version guard or any upload — so an operator who
        // ran `--publishers cargo` (or `--skip=blob`) does NOT push blobs to
        // the store. Recorded as `Skipped(Deselected)` so the run summary
        // counts it; never silent.
        if ctx.publisher_deselected("blob") {
            let line = ctx.deselected_reason("blob");
            ctx.logger("blob").status(&line);
            record_blob_deselected(ctx);
            return Ok(());
        }

        // Refuse to upload a non-release version (snapshot / dirty /
        // 0.0.0-sentinel) to an object store. A `--skip=publish` run reaches
        // BlobStage without the publish guard ever firing, so the same shared
        // guard runs here too — BEFORE any byte is uploaded. No-op in
        // dry-run/snapshot; `--allow-snapshot-publish` downgrades to a warning.
        {
            let log = ctx.logger("blob");
            let targets = blob_destinations(ctx);
            anodizer_core::version::guard_release_version(ctx, &log, "blob", &targets)?;
        }

        // BlobStage is Assets-group and writes its own outcome into
        // `ctx.publish_report`. It runs BEFORE PublishStage (and the
        // SnapcraftPublishStage submitter) so a required-blob failure is
        // already recorded when the Submitter loop and snapcraft consult
        // `submitter_gate_closed()` (via `any_failed(Assets,
        // required_only=true)`) and gate the one-way doors. Because blob is
        // first, `ctx.publish_report` is normally `None` at entry; the
        // `record_blob_*` helpers create it on demand. The dispatcher then
        // SEEDS its report from this blob outcome rather than starting empty.
        //
        // Per-target failure becomes `PublisherOutcome::Failed(_)` + Ok(()).
        // Catastrophic errors (missing required config, malformed
        // provider, IO impossible at the stage boundary) still bubble up
        // via the `?` operator in `run_with_evidence` -> `prepare_jobs`.

        // Gate check: defends against any required failure already recorded in
        // `ctx.publish_report` before blob runs (none in the standard pipeline,
        // where blob is first; non-`None` only if a future earlier Assets stage
        // records one). Skip the upload when the authoritative gate predicate
        // has closed — pushing more bytes to an already-broken release just
        // leaves orphaned assets pointing at a release that will not complete.
        // Uses the same `submitter_gate_closed` predicate as every other gate
        // site so the rule has a single source of truth.
        let gate_submitter = ctx.options.gate_submitter.unwrap_or(true);
        if gate_submitter
            && let Some(report) = ctx.publish_report()
            && report.submitter_gate_closed()
        {
            let log = ctx.logger("blob");
            log.status("blob skipped via submitter-gate");
            record_blob_gated(ctx);
            return Ok(());
        }

        let report = self.run_report(ctx)?;
        if let Some(exec_result) = report.exec {
            // Surface the real cause at default visibility: a failed upload
            // otherwise recorded only `blob  Assets  required  failed` with no
            // reason in the log (the cause lived solely in report.json). A
            // failure banner stays at `status` per the log register.
            if let Err(err) = &exec_result {
                ctx.logger("blob")
                    .status(&format!("blob upload failed: {err:#}")); // status-ok: failure banner
            }
            // Aggregated `required` across every blob config on every
            // selected crate: when ANY blob config opts in
            // (`required: true`), the recorded outcome carries
            // `required = true` so a failed upload trips the submitter
            // gate (`any_failed(Assets, required_only=true)`) and the
            // CLI's required-failures exit-code gate. See
            // `BlobConfig.required` rustdoc for the semantics.
            let derived_required = derive_blob_required(ctx);
            // Every object was already present byte-identical and nothing new
            // was uploaded: an idempotent re-run, recorded as a SKIP.
            let fully_idempotent_skip =
                exec_result.is_ok() && report.targets.is_empty() && report.skipped_identical > 0;
            record_blob_result(
                ctx,
                &report.targets,
                &exec_result,
                derived_required,
                fully_idempotent_skip,
            );
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
///
/// `uploaded` is the structured per-object capture (provider, bucket,
/// key, region, endpoint) — both the operator-readable
/// `provider://bucket/key` URL list (rendered into
/// `evidence.artifact_paths`) and the structured form (encoded into
/// `evidence.extra.blob_targets` via [`crate::publisher::encode_blob_targets`])
/// are derived from it so the rollback path can issue real
/// `ObjectStore::delete` calls.
pub(crate) fn record_blob_result(
    ctx: &mut Context,
    uploaded: &[BlobTarget],
    exec_result: &Result<()>,
    required: bool,
    fully_idempotent_skip: bool,
) {
    let outcome = match exec_result {
        Ok(()) if fully_idempotent_skip => PublisherOutcome::Skipped(SkipReason::AlreadyPublished),
        Ok(()) => PublisherOutcome::Succeeded,
        Err(e) => PublisherOutcome::Failed(format!("{e:#}")),
    };
    let evidence = match exec_result {
        Ok(()) if !uploaded.is_empty() => {
            let mut e = anodizer_core::PublishEvidence::new("blob");
            e.primary_ref = Some(blob_target_url(&uploaded[0]));
            e.artifact_paths = uploaded
                .iter()
                .map(|t| std::path::PathBuf::from(blob_target_url(t)))
                .collect();
            e.extra = crate::publisher::encode_blob_targets(uploaded);
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

/// Record a `Skipped(SubmitterGated)` row for the blob stage when an
/// upstream required publisher failed and the gate closed before BlobStage
/// could upload. Mirrors `record_blob_result`'s report-init discipline.
///
/// The recorded `required` flag carries `derive_blob_required(ctx)` so the
/// row reflects the operator's configured intent even though no upload was
/// attempted — a gated skip is not a failure, so it never trips the
/// required-failures exit gate, but the flag keeps the report row honest.
pub(crate) fn record_blob_gated(ctx: &mut Context) {
    let required = derive_blob_required(ctx);
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
        outcome: PublisherOutcome::Skipped(SkipReason::SubmitterGated),
        evidence: None,
    });
}

/// Record a `Skipped(Deselected)` row for the blob stage when the operator
/// excluded it via `--skip=blob` or omitted it from a `--publishers`
/// allowlist. Mirrors [`record_blob_gated`]'s report-init discipline.
///
/// The recorded `required` flag carries [`derive_blob_required`] so the row
/// reflects the operator's configured intent even though no upload was
/// attempted — a deselected skip is not a failure, so it never trips the
/// required-failures exit gate, but the flag keeps the report row honest.
pub(crate) fn record_blob_deselected(ctx: &mut Context) {
    let required = derive_blob_required(ctx);
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
        outcome: PublisherOutcome::Skipped(SkipReason::Deselected),
        evidence: None,
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

/// The `provider://bucket` destinations the blob stage was about to upload to
/// across every selected crate's `blobs:` config. Names the targets in the
/// non-release guard's error so the operator sees exactly which buckets a
/// snapshot version was about to reach. Best-effort orientation only — the raw
/// configured `provider`/`bucket` strings (templates unrendered) are enough to
/// identify the destination without building object stores.
///
/// Walks the same `crate_universe` (top-level `crates` + every
/// `workspaces[].crates`) the guard predicate uses, so a pure-workspace config
/// whose `blobs:` lives only under a workspace crate still names its bucket
/// rather than reporting `(none configured)`.
fn blob_destinations(ctx: &Context) -> Vec<String> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter_map(|c| c.blobs.as_ref())
        .flat_map(|configs| configs.iter())
        .map(|cfg| format!("{}://{}", cfg.provider, cfg.bucket))
        .collect()
}

impl BlobStage {
    /// Execute the blob upload like [`Stage::run`] but return the list of
    /// per-object [`BlobTarget`] tuples that were actually uploaded.
    /// `BlobPublisher::run` calls this so `PublishEvidence` records the
    /// structured (provider, bucket, key, region, endpoint) shape needed
    /// for the rollback DELETE path, plus the operator-readable
    /// `provider://bucket/key` URL list. The prior pre-upload capture
    /// produced a rollback checklist that referenced files which never
    /// existed when a mid-stream upload failed.
    ///
    /// On error: returns the list of files that succeeded before the
    /// failure (via [`anyhow::Error::downcast`] handoff), so the caller
    /// can still emit a partial rollback checklist. The current
    /// implementation runs the upload phase atomically per job; partial
    /// success is captured up to the failing job's boundary.
    pub(crate) fn run_with_evidence(&self, ctx: &mut Context) -> Result<Vec<BlobTarget>> {
        let report = self.run_report(ctx)?;
        if let Some(r) = report.exec {
            r?;
        }
        Ok(report.targets)
    }

    /// Like [`Self::run_with_evidence`] but splits the catastrophic
    /// pre-flight / setup errors (returned as the outer `Result::Err`,
    /// matching the public `run_with_evidence` contract) from the
    /// upload-phase outcome.
    ///
    /// Return shape (see [`BlobRunReport`]):
    /// - `Err(_)`: catastrophic — config validation failed, runtime
    ///   construction failed, etc. Bubbled out of `Stage::run` as the
    ///   pipeline-failing error.
    /// - `exec == None`: no work was attempted (snapshot-skip, no
    ///   configured crates, every job was disabled or had no files).
    ///   `Stage::run` does NOT append a `PublisherResult` in this case.
    /// - `exec == Some(Ok(()))`: at least one job ran and every
    ///   upload succeeded (some may have been idempotent skips —
    ///   `skipped_identical` counts those).
    /// - `exec == Some(Err(_))`: at least one job ran and at least
    ///   one upload failed; `targets` carries the partial-success list
    ///   captured up to the failure.
    ///
    /// `Stage::run` consumes `exec` to decide whether to record a
    /// `PublisherOutcome::Succeeded` / `Failed(_)` / `Skipped(AlreadyPublished)`
    /// entry.
    fn run_report(&self, ctx: &mut Context) -> Result<BlobRunReport> {
        let log = ctx.logger("blob");
        if ctx.skip_in_snapshot(&log, "blob") {
            return Ok(BlobRunReport::no_work());
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
            return Ok(BlobRunReport::no_work());
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

        // Serial: render every config, build stores, collect jobs.
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

                // `blobs[].if:` conditional gate.
                let proceed = anodizer_core::config::evaluate_if_condition(
                    blob_cfg.if_condition.as_deref(),
                    &format!("blob config for crate {}", krate.name),
                    |t| ctx.render_template(t),
                )?;
                if !proceed {
                    log.status(&format!(
                        "blob config for crate {} skipped — `if` condition evaluated falsy",
                        krate.name
                    ));
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

                // Render region / endpoint here so the publisher can
                // capture the post-template values into BlobTarget for
                // the rollback DELETE path. `build_store` re-renders
                // internally; doing it again here is cheap (Tera caches
                // the parsed AST) and keeps the rollback evidence
                // self-contained — no template re-evaluation at rollback
                // time, no dependency on the live ctx vars.
                let rendered_region = blob_cfg
                    .region
                    .as_deref()
                    .map(|r| {
                        ctx.render_template(r).with_context(|| {
                            format!(
                                "blobs[{}]: render region template for crate {}",
                                config_label, krate.name
                            )
                        })
                    })
                    .transpose()?;
                let rendered_endpoint = blob_cfg
                    .endpoint
                    .as_deref()
                    .map(|e| {
                        ctx.render_template(e).with_context(|| {
                            format!(
                                "blobs[{}]: render endpoint template for crate {}",
                                config_label, krate.name
                            )
                        })
                    })
                    .transpose()?;

                // Default `{{ ProjectName }}/{{ Tag }}`,
                // expressed in Tera syntax (no leading `.`).
                // Anodizer's renderer accepts both forms, so a YAML lifted from
                // a config that overrides `directory:` keeps working.
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

                let artifacts = collect_artifacts(ctx, blob_cfg, &krate.name, &log);
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
                            "(dry-run) would upload {} → {}",
                            local_path.display(),
                            remote,
                        ));
                    }
                    continue;
                }

                // Per-file detail belongs behind `-v`: at default verbosity a
                // release uploading hundreds of objects would otherwise bury
                // the rest of the run. The single status summary (with real
                // uploaded/skipped counts) is emitted per job after the upload
                // loop below. The serial prep order keeps the verbose lines
                // deterministic even though the actual upload runs in parallel.
                for (local_path, remote_key) in &upload_items {
                    let remote = format_remote_path(
                        provider,
                        &rendered_bucket,
                        &rendered_directory,
                        remote_key,
                    );
                    log.verbose(&format!("uploading {} → {}", local_path.display(), remote));
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
                    rendered_region,
                    rendered_endpoint,
                    upload_items,
                    store,
                    put_opts_per_item,
                    parallelism_inner,
                    client_kms,
                });
            }
        }

        if jobs.is_empty() {
            return Ok(BlobRunReport::no_work());
        }

        // Parallel across configs: each worker runs its own upload loop
        // (which itself has intra-config per-file concurrency via tokio).
        // Bounded by the global parallelism so we don't fan out unbounded
        // across both axes simultaneously.
        //
        // One tokio runtime is shared across every job — N parallel jobs
        // would otherwise allocate N independent thread pools.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("anodizer-blob")
            .build()
            .context("blob: failed to construct tokio runtime")?;
        let runtime_ref = &runtime;
        // Shared accumulator of uploaded [`BlobTarget`]s across every
        // job. `upload_files_owned` records each successful upload on
        // its own task; the per-job wrapper translates the returned
        // object keys into structured `BlobTarget` tuples (provider,
        // bucket, key, region, endpoint) before appending to the shared
        // list. On failure the partial list is preserved so
        // PublishEvidence captures only files that landed — and carries
        // the structured shape needed for the rollback DELETE path.
        let uploaded_targets: std::sync::Arc<std::sync::Mutex<Vec<BlobTarget>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // Count of objects skipped because an identical copy already existed.
        // Drives the idempotent-skip outcome when nothing new was uploaded.
        let skipped_identical: std::sync::Arc<std::sync::atomic::AtomicUsize> =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let job_log = log.clone();
        let run_job = |job: &BlobJob| -> Result<()> {
            match upload_files_owned(
                runtime_ref,
                Arc::clone(&job.store),
                job.upload_items.clone(),
                job.rendered_directory.clone(),
                job.put_opts_per_item.clone(),
                job.parallelism_inner,
                job.client_kms.clone(),
                &job_log,
            ) {
                Ok(report) => {
                    // One factual default-verbosity line per job, collapsing
                    // the per-file `uploading …`/`skipping …` firehose (now
                    // verbose-only). Counts come straight from this job's
                    // report, so per-crate mode reports one summary per
                    // published crate's job with that crate's own counts.
                    let destination = crate::upload::format_remote_prefix(
                        job.provider_display,
                        &job.rendered_bucket,
                        &job.rendered_directory,
                    );
                    job_log.status(&crate::upload::blob_upload_summary(
                        report.uploaded.len(),
                        report.skipped_identical.len(),
                        &destination,
                    ));
                    skipped_identical.fetch_add(
                        report.skipped_identical.len(),
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    let mut acc = anodizer_core::parallel::lock_recover(
                        &uploaded_targets,
                        &job_log,
                        "blob targets",
                    );
                    // Only freshly-uploaded (or overwritten) keys become
                    // rollback targets; skipped-identical objects predate
                    // this run and must not be deleted on rollback.
                    for key in report.uploaded {
                        acc.push(BlobTarget {
                            provider: job.provider_display.to_string(),
                            bucket: job.rendered_bucket.clone(),
                            key,
                            region: job.rendered_region.clone(),
                            endpoint: job.rendered_endpoint.clone(),
                        });
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
        // Sort by URL so the resulting evidence is deterministic across
        // runs of the same job graph.
        let mut targets =
            anodizer_core::parallel::lock_recover(&uploaded_targets, &log, "blob targets").clone();
        targets.sort_by_key(blob_target_url);

        // Collapse `Vec<()>` -> `()` so the inner Result has the same
        // shape as `Stage::run`'s return — the per-job successes have
        // already been folded into the shared `uploaded_targets`
        // accumulator above.
        let exec = result.map(|_| ());
        Ok(BlobRunReport {
            targets,
            exec: Some(exec),
            skipped_identical: skipped_identical.load(std::sync::atomic::Ordering::SeqCst),
        })
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{BlobConfig, Config, CrateConfig, StringOrBool};
    use anodizer_core::context::ContextOptions;

    /// One crate carrying one blob config — the single-crate shape.
    fn config_with_blob(crate_name: &str, blob: BlobConfig) -> Config {
        Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: crate_name.to_string(),
                path: ".".to_string(),
                blobs: Some(vec![blob]),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A dry-run context over `config` with the template vars the default
    /// `directory:` (`{{ ProjectName }}/{{ Tag }}`) needs.
    fn dry_run_ctx(config: Config) -> Context {
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "demo");
        ctx.template_vars_mut().set("IsSnapshot", "false");
        ctx
    }

    fn add_archive(ctx: &mut Context, crate_name: &str, path: &str) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from(path),
            target: None,
            crate_name: crate_name.to_string(),
            metadata: Default::default(),
            size: None,
        });
    }

    // -------------------------------------------------------------------
    // validate_only — config-validation entry point. Empty required fields
    // bail before any store is built.
    // -------------------------------------------------------------------

    #[test]
    fn validate_only_bails_on_empty_provider() {
        let cfg = BlobConfig {
            provider: String::new(),
            bucket: "b".to_string(),
            ..Default::default()
        };
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let err = validate_only(&cfg, &ctx).expect_err("empty provider must bail");
        assert!(
            err.to_string().contains("provider is required"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_only_bails_on_empty_bucket() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: String::new(),
            ..Default::default()
        };
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let err = validate_only(&cfg, &ctx).expect_err("empty bucket must bail");
        assert!(err.to_string().contains("bucket is required"), "got: {err}");
    }

    #[test]
    fn validate_only_rejects_unknown_provider_after_render() {
        // A bogus literal provider must be caught by Provider::parse inside
        // validate_only, naming the bad value and the valid set.
        let cfg = BlobConfig {
            provider: "gss".to_string(),
            bucket: "b".to_string(),
            ..Default::default()
        };
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let err = validate_only(&cfg, &ctx).expect_err("bad provider must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("gss") && msg.contains("s3"),
            "error must name the bad value and the valid set; got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // run_report pre-flight — a literal (non-templated) provider typo must
    // fail FAST, before any store build or upload, as a catastrophic Err
    // bubbling out of Stage::run.
    // -------------------------------------------------------------------

    #[test]
    fn literal_provider_typo_fails_preflight() {
        let cfg = BlobConfig {
            provider: "azureblob".to_string(), // typo: valid is "azblob"
            bucket: "b".to_string(),
            ..Default::default()
        };
        let mut ctx = dry_run_ctx(config_with_blob("c", cfg));
        add_archive(&mut ctx, "c", "dist/c-v1.0.0.tar.gz");
        let err = BlobStage
            .run(&mut ctx)
            .expect_err("provider typo must bail");
        assert!(
            err.to_string().contains("azureblob"),
            "preflight error must name the offending provider; got: {err}"
        );
    }

    // -------------------------------------------------------------------
    // skip / if gates — a skipped or falsy-`if` blob config produces no
    // jobs and no publish-report entry (no work attempted).
    // -------------------------------------------------------------------

    #[test]
    fn skip_true_produces_no_publish_report_entry() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let mut ctx = dry_run_ctx(config_with_blob("c", cfg));
        add_archive(&mut ctx, "c", "dist/c.tar.gz");
        BlobStage.run(&mut ctx).expect("skipped config is Ok");
        assert!(
            ctx.publish_report.is_none(),
            "a skipped blob config attempts no work → no report entry"
        );
    }

    #[test]
    fn if_condition_falsy_skips_config() {
        // `if: "{{ false }}"` → the config is gated out before any store
        // build; no work, no report entry.
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            if_condition: Some("{{ IsSnapshot }}".to_string()),
            ..Default::default()
        };
        let mut ctx = dry_run_ctx(config_with_blob("c", cfg));
        // IsSnapshot=false (set by dry_run_ctx) → condition falsy → skipped.
        add_archive(&mut ctx, "c", "dist/c.tar.gz");
        BlobStage.run(&mut ctx).expect("falsy-if config is Ok");
        assert!(
            ctx.publish_report.is_none(),
            "falsy `if` gates the config out → no work attempted"
        );
    }

    // -------------------------------------------------------------------
    // snapshot skip — `--snapshot` runs short-circuit the whole stage.
    // -------------------------------------------------------------------

    #[test]
    fn snapshot_run_skips_stage_entirely() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            ..Default::default()
        };
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config_with_blob("c", cfg), opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "demo");
        ctx.template_vars_mut().set("IsSnapshot", "true");
        add_archive(&mut ctx, "c", "dist/c.tar.gz");

        BlobStage.run(&mut ctx).expect("snapshot run is Ok");
        assert!(
            ctx.publish_report.is_none(),
            "snapshot short-circuits the blob stage before any job is built"
        );
    }

    // -------------------------------------------------------------------
    // non-release version guard — wiring proof.
    // -------------------------------------------------------------------

    /// Pins that the non-release version guard is WIRED into `BlobStage::run`,
    /// not merely that the shared `guard_release_version` works in isolation.
    /// Drives the real `Stage::run` entrypoint with a configured blob and a
    /// `0.0.0~SNAPSHOT-<sha>` version on a real-release (non-snapshot,
    /// non-dry-run) ctx, asserting it bails BEFORE any upload (`publish_report`
    /// stays `None`) with an error naming the stage, version, the bucket, and
    /// the override flag. Deleting the `guard_release_version` call at the
    /// `BlobStage::run` call site makes this test fail (the run would proceed
    /// past the guard into preflight/upload).
    #[test]
    fn blob_stage_run_bails_on_non_release_version() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "releases".to_string(),
            ..Default::default()
        };
        // Real release: NOT snapshot, NOT dry-run, so the guard is live.
        let mut ctx = Context::new(config_with_blob("c", cfg), ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "demo");
        ctx.template_vars_mut().set("IsSnapshot", "false");
        ctx.template_vars_mut()
            .set("Version", "0.0.0~SNAPSHOT-d7813f0");
        add_archive(&mut ctx, "c", "dist/c.tar.gz");

        let err = BlobStage
            .run(&mut ctx)
            .expect_err("a non-release version must bail at BlobStage::run");
        let msg = err.to_string();
        assert!(msg.contains("blob"), "error must name the stage: {msg}");
        assert!(
            msg.contains("0.0.0~SNAPSHOT-d7813f0"),
            "error must name the offending version: {msg}",
        );
        assert!(
            msg.contains("s3://releases"),
            "error must name the destination bucket: {msg}",
        );
        assert!(
            msg.contains("--allow-snapshot-publish"),
            "error must tell the operator how to override: {msg}",
        );
        assert!(
            ctx.publish_report.is_none(),
            "guard must abort BEFORE any upload or report entry",
        );
    }

    /// A pure-workspace config (crates live ONLY under `workspaces[].crates`,
    /// not top-level `crates`) must still name the workspace crate's bucket in
    /// the guard error — `blob_destinations` walks the same `crate_universe` the
    /// predicate does, so the operator is never left with `(none configured)`
    /// exactly when a workspace-only blob was about to be hit.
    #[test]
    fn blob_stage_run_names_workspace_bucket_on_non_release_version() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "ws-releases".to_string(),
            ..Default::default()
        };
        let config = Config {
            project_name: "demo".to_string(),
            workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![CrateConfig {
                    name: "wscrate".to_string(),
                    path: ".".to_string(),
                    blobs: Some(vec![cfg]),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        // Real release: NOT snapshot, NOT dry-run, so the guard is live.
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "demo");
        ctx.template_vars_mut().set("IsSnapshot", "false");
        ctx.template_vars_mut()
            .set("Version", "0.0.0~SNAPSHOT-d7813f0");

        let err = BlobStage
            .run(&mut ctx)
            .expect_err("a non-release version must bail at BlobStage::run");
        let msg = err.to_string();
        assert!(
            msg.contains("s3://ws-releases"),
            "error must name the WORKSPACE crate's bucket, not '(none configured)': {msg}",
        );
        assert!(
            !msg.contains("(none configured)"),
            "a configured workspace blob must never report '(none configured)': {msg}",
        );
    }

    // -------------------------------------------------------------------
    // selected_crates filtering — only the selected crate's blob config is
    // considered (workspace per-crate selection).
    // -------------------------------------------------------------------

    #[test]
    fn selected_crates_excludes_unselected_blob_configs() {
        // Two crates, each with a blob config — but only `alpha` is selected.
        // `beta` carries a provider TYPO that would bail preflight if it were
        // considered. Selecting `alpha` only must let the run succeed,
        // proving `beta` was filtered out before preflight.
        let config = Config {
            project_name: "demo".to_string(),
            crates: vec![
                CrateConfig {
                    name: "alpha".to_string(),
                    path: ".".to_string(),
                    blobs: Some(vec![BlobConfig {
                        provider: "s3".to_string(),
                        bucket: "a".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                CrateConfig {
                    name: "beta".to_string(),
                    path: ".".to_string(),
                    blobs: Some(vec![BlobConfig {
                        provider: "BOGUS".to_string(),
                        bucket: "b".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let opts = ContextOptions {
            dry_run: true,
            selected_crates: vec!["alpha".to_string()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "demo");
        ctx.template_vars_mut().set("IsSnapshot", "false");
        add_archive(&mut ctx, "alpha", "dist/alpha.tar.gz");

        BlobStage
            .run(&mut ctx)
            .expect("only the selected (valid) crate is considered");
    }

    // -------------------------------------------------------------------
    // KMS scheme mismatch — caught in the serial prep phase before any
    // upload (gcpkms:// against an s3 bucket). This is a per-job
    // catastrophic Err that bubbles out of Stage::run.
    //
    // Uses a NON-dry-run ctx: the KMS validate-match runs in the store-build
    // path, which dry-run skips. No real store/network is reached because
    // validate_kms_provider_match bails first.
    // -------------------------------------------------------------------

    #[test]
    fn kms_scheme_mismatch_bails_before_upload() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            kms_key: Some("gcpkms://projects/p/locations/l/keyRings/k/cryptKeys/c".to_string()),
            ..Default::default()
        };
        let mut ctx = Context::new(config_with_blob("c", cfg), ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "demo");
        ctx.template_vars_mut().set("IsSnapshot", "false");
        add_archive(&mut ctx, "c", "dist/c.tar.gz");

        let err = BlobStage
            .run(&mut ctx)
            .expect_err("gcpkms:// on an s3 bucket must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("not compatible") && msg.contains("awskms://"),
            "KMS mismatch must surface the scheme gate before upload; got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // No-files path — a blob config whose crate has no artifacts attempts
    // no upload and records no report entry (warn-and-continue).
    // -------------------------------------------------------------------

    #[test]
    fn no_files_to_upload_records_no_entry() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            ..Default::default()
        };
        // No artifacts added for crate "c" → upload_items empty → warn + skip.
        let mut ctx = dry_run_ctx(config_with_blob("c", cfg));
        BlobStage.run(&mut ctx).expect("no-files run is Ok");
        assert!(
            ctx.publish_report.is_none(),
            "no files → no job built → no publish-report entry"
        );
    }

    // -------------------------------------------------------------------
    // Dry-run remote-path assembly — the dry-run branch logs the assembled
    // provider://bucket/directory/key for each file WITHOUT building a
    // store. We assert the captured log lines carry the templated target
    // (region/endpoint/custom directory all rendered).
    // -------------------------------------------------------------------

    #[test]
    fn dry_run_logs_templated_remote_target() {
        let capture = anodizer_core::log::LogCapture::new();
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "{{ ProjectName }}-releases".to_string(),
            directory: Some("artifacts/{{ Tag }}".to_string()),
            ..Default::default()
        };
        let mut ctx = dry_run_ctx(config_with_blob("c", cfg));
        ctx.with_log_capture(capture.clone());
        add_archive(&mut ctx, "c", "dist/myapp-v1.0.0.tar.gz");

        BlobStage.run(&mut ctx).expect("dry-run is Ok");

        let lines: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        assert!(
            lines.iter().any(|l| l.contains("(dry-run)")
                && l.contains("demo-releases/artifacts/v1.0.0/myapp-v1.0.0.tar.gz")),
            "dry-run must log the templated bucket+directory+key target; got: {lines:?}"
        );
        assert!(
            ctx.publish_report.is_none(),
            "dry-run uploads nothing → no publish-report entry"
        );
    }
}
