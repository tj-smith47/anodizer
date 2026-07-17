use std::ops::ControlFlow;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryLog, is_retriable, retry_sync_deadline};
use anodizer_core::run::run_capture_timeout;
use anodizer_core::stage::Stage;
use anodizer_core::{
    PublishEvidence, PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
};

use crate::arch::triple_to_snap_arch;
use crate::command::{
    first_channel_rejected_for_prerelease_snap, is_content_dedup_rejection, is_retriable_snap_push,
    missing_channels_for_version, resolve_effective_channels, snap_revision_for_version,
    snapcraft_list_revisions_command, snapcraft_release_command, snapcraft_upload_command,
};
use crate::targets::{SnapcraftTarget, collect_snapcraft_targets};

/// Wall-clock bound on a single `snapcraft upload` attempt. A Snap Store upload
/// that stalls (unreachable store, hung TLS handshake, a snapcraft prompt
/// blocking on stdin) would otherwise hang the entire release forever — the
/// store-side analogue of the bounded release-backend HTTP timeout. Sized
/// generously for a multi-MB snap on a slow link; on expiry the whole snapcraft
/// process subtree is killed and the attempt retries within the upload budget.
const SNAPCRAFT_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Wall-clock bound on the `snapcraft list-revisions` existence probe. The probe
/// only reads a small revision table, so a much shorter bound suffices; on
/// timeout it is treated like any other probe failure (proceed to upload rather
/// than falsely skip a genuine first publish).
const SNAPCRAFT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

// ---------------------------------------------------------------------------
// SnapcraftPublishStage — uploads previously built .snap artifacts
// ---------------------------------------------------------------------------
//
// `SnapcraftPublishStage` is the load-bearing snapcraft runner. Following
// the `BlobStage` pattern (commit 026c854), the stage writes its own
// `PublisherResult` directly into `ctx.publish_report` so the Submitter gate
// (and any downstream consumers, e.g. announce-gating, `--rollback-only
// --from-run`) observes outcomes uniformly. A parallel trait-based
// `SnapcraftPublisher` registration would fire `snapcraft upload` a
// second time per release — see the doc comment on
// `stage-publish::registry::configured_publishers`.

pub struct SnapcraftPublishStage;

impl Stage for SnapcraftPublishStage {
    fn name(&self) -> &str {
        "snapcraft-publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("snapcraft-publish");

        // Computed once up front: `SnapcraftConfig.required` is pure config
        // and cannot change mid-run, so every `record_snapcraft_result` call
        // site below (gated, deselected, already-published, terminal) shares
        // the same derived flag.
        let required = derive_snapcraft_required(ctx);

        // Operator-selection gate. SnapcraftPublishStage performs an external,
        // irreversible Snap Store upload but runs as a pipeline stage OUTSIDE
        // the trait-based dispatch chokepoint, so the uniform `--skip` /
        // `--publishers` filter that governs every dispatched publisher does
        // not reach it. Consult `publisher_deselected("snapcraft-publish")`
        // here — BEFORE the snapshot/submitter gates or any upload — so an
        // operator who ran `--publishers cargo` (or `--skip=snapcraft-publish`)
        // does NOT push a snap to the store. Recorded as `Skipped(Deselected)`
        // so the run summary counts it; never silent.
        if ctx.publisher_deselected("snapcraft-publish") {
            log.status(&ctx.deselected_reason("snapcraft-publish"));
            record_snapcraft_result(
                ctx,
                None,
                PublisherOutcome::Skipped(SkipReason::Deselected),
                required,
            );
            return Ok(());
        }

        if ctx.skip_in_snapshot(&log, "snapcraft-publish") {
            // Mirror BlobStage's discipline: snapshot-skip leaves
            // `publish_report` untouched. Recording a `Skipped(Snapshot)`
            // entry here would asymmetrically gate
            // `AnnounceGate::AllPublishers` against snapcraft alone if the
            // announce snapshot-skip-first guard is ever relaxed.
            return Ok(());
        }

        // Submitter-gate check: SnapcraftPublishStage is a Submitter-group
        // surface (irreversible snap-store upload — once a revision is
        // pushed there is no programmatic rollback). It runs as its own
        // stage AFTER the trait-based publisher dispatch, so the report it
        // consults already carries every Assets/Manager outcome AND every
        // earlier Submitter outcome (cargo, winget, chocolatey). Consult the
        // single authoritative `submitter_gate_closed` predicate — the same
        // one the in-dispatch Submitter loop uses — so a required failure in
        // ANY upstream group, including a required cargo (Submitter) failure,
        // skips the snapcraft upload. Without the intra-Submitter arm a
        // failed required cargo publish would still let snapcraft push.
        let gate_submitter = ctx.options.gate_submitter.unwrap_or(true);
        if gate_submitter
            && let Some(report) = ctx.publish_report()
            && report.submitter_gate_closed()
        {
            log.status("snapcraft-publish skipped via submitter-gate");
            record_snapcraft_result(
                ctx,
                None,
                PublisherOutcome::Skipped(SkipReason::SubmitterGated),
                required,
            );
            return Ok(());
        }

        let selected = ctx.options.selected_crates.clone();

        // Collect crates that have snapcraft config with publish: true
        let crates: Vec<CrateConfig> = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.snapcrafts.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            // No work attempted (empty crates) — leave publish_report
            // untouched, matching BlobStage's discipline.
            return Ok(());
        }

        // Collect all snap artifacts that were built
        let snap_artifacts: Vec<Artifact> = ctx
            .artifacts
            .by_kind(ArtifactKind::Snap)
            .into_iter()
            .cloned()
            .collect();

        if snap_artifacts.is_empty() {
            // No work attempted (empty snap_artifacts) — leave
            // publish_report untouched, matching BlobStage's discipline.
            return Ok(());
        }

        // Pre-submitter verify-release gate: the same post-publish
        // content check the in-dispatch Submitter loop consults, run here
        // too because snapcraft is a Submitter-group surface that executes
        // as its own pipeline stage OUTSIDE that loop. Deferred until after
        // the crates/snap_artifacts emptiness checks above: a release with
        // no snapcraft work to do must leave `publish_report` untouched
        // (BlobStage's "no work, no record" contract) rather than trigger a
        // live GH asset-verification fetch and possibly record a phantom
        // `Skipped(VerifyGateBlocked)` for a stage that never had anything
        // to upload.
        // `ensure_verify_gate_evaluated` is the shared run-once-per-release
        // coordination point: on a release that also configures a
        // trait-dispatched Submitter (cargo, winget, ...), the in-dispatch
        // loop already ran the live check and persisted its verdict, so
        // this call is a no-op re-read. On a release configuring ONLY
        // `snapcraft:` — no other Submitter-group publisher ever reaches
        // the in-dispatch loop, so its lazy eval never fires — this is the
        // first and only place the gate runs; without this call a
        // snapcraft-only release would push an unverified snap.
        if gate_submitter {
            let mut report = ctx.publish_report.take().unwrap_or_default();
            anodizer_core::publish_report::ensure_verify_gate_evaluated(
                ctx,
                &mut report,
                "snapcraft-publish",
            );
            let blocked = report.verify_gate_blocked;
            ctx.publish_report = Some(report);
            if blocked {
                log.status(
                    "snapcraft-publish skipped — blocked by the pre-submitter verify-release gate",
                );
                record_snapcraft_result(
                    ctx,
                    None,
                    PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked),
                    required,
                );
                return Ok(());
            }
        }

        // Pre-pass: render every config's `publish.skip` template uniformly
        // BEFORE any upload begins. A template-render failure in this pass
        // is a config error, not an upload outcome — it must fast-fail as a
        // stage error consistently, regardless of which crate iterates
        // first. Folding it into a `Failed(_)` PublisherResult would make
        // `publish_report.json` misrepresent the same bug as an upload
        // failure for some crate orderings and a stage abort for others.
        let skip_decisions = render_skip_decisions(ctx, &crates)?;

        // Capture the resolved per-target snapshot BEFORE we start
        // uploading so a mid-stream failure still leaves the operator a
        // manual channel-management pointer for each snap we attempted to
        // push. The snapshot also feeds
        // `PublishEvidence::extra.snapcraft_targets` on success so
        // `--rollback-only --from-run` consumers can decode the recorded
        // shape.
        let mut targets = collect_snapcraft_targets(ctx);

        let SnapUploadOutcome {
            attempted,
            skipped_already_published,
            held_for_review,
            result: exec_result,
        } = run_uploads(
            ctx,
            &crates,
            &snap_artifacts,
            &skip_decisions,
            &log,
            &mut targets,
        );

        if !attempted {
            // Nothing was uploaded this run. Two shapes land here:
            //
            // 1. Every applicable snap's version was already published (the
            //    idempotency probe skipped them) — record an
            //    AlreadyPublished SKIP so a re-run reports the true state
            //    instead of defaulting to Succeeded with no evidence.
            // 2. Genuinely no work (every config `publish: false`, every
            //    entry `skip:`, or dry-run) — mirror BlobStage's "no work,
            //    no record" contract and bubble `exec_result`.
            //
            // The closure cannot have failed before flipping `attempted`
            // (skip-templates pre-rendered; upload errors set `attempted`
            // first), so `exec_result` is `Ok(())` here.
            if exec_result.is_ok() && skipped_already_published > 0 {
                record_snapcraft_result(
                    ctx,
                    None,
                    PublisherOutcome::Skipped(SkipReason::AlreadyPublished),
                    required,
                );
            }
            return exec_result;
        }

        let outcome = match &exec_result {
            Ok(()) => PublisherOutcome::Succeeded,
            Err(e) => PublisherOutcome::Failed(format!("{e:#}")),
        };
        let evidence = matches!(outcome, PublisherOutcome::Succeeded)
            .then(|| build_snapcraft_evidence(&targets));
        record_snapcraft_result(ctx, evidence, outcome, required);
        // End-of-stage accountability for review holds: a per-upload warn can
        // scroll away inside a long publish log, and the store's eventual
        // decline arrives only by email — so the unresolved state gets one
        // unmissable rollup line too. verify-release additionally probes the
        // store's channel map and fails the gate while the version is absent.
        if !held_for_review.is_empty() {
            log.warn(&format!(
                "{} snap upload(s) HELD for Snap Store manual review — store release NOT \
                 verified: {}",
                held_for_review.len(),
                held_for_review.join(", ")
            ));
        }
        // Per-target upload errors are reported via PublisherResult; they
        // must NOT bail the pipeline because announce-gating and the
        // Submitter gate downstream depend on this stage returning Ok(()).
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pre-pass: render every config's `publish.skip` template upfront so a
// template error fast-fails as a stage error before any upload begins.
// ---------------------------------------------------------------------------

/// Per-config skip flag, indexed parallel to the
/// `(crate_index, snap_cfg_index)` ordering of
/// `crates[].snapcrafts[]`. `run_uploads` indexes into this Vec rather
/// than re-rendering the template inside the upload loop.
fn render_skip_decisions(ctx: &Context, crates: &[CrateConfig]) -> Result<Vec<bool>> {
    let mut decisions = Vec::new();
    for krate in crates {
        let Some(snap_configs) = krate.snapcrafts.as_ref() else {
            continue;
        };
        for snap_cfg in snap_configs {
            let skip = if let Some(ref d) = snap_cfg.skip {
                d.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| {
                        format!(
                            "snapcraft: render publish.skip template for crate {}",
                            krate.name
                        )
                    })?
            } else {
                false
            };
            // Treat `if:` as another skip-decision: when the gate is falsy,
            // skip this snap from the publish phase too.
            let if_skip = !anodizer_core::config::evaluate_if_condition(
                snap_cfg.if_condition.as_deref(),
                &format!("snapcraft publish for crate '{}'", krate.name),
                |t| ctx.render_template(t),
            )?;
            decisions.push(skip || if_skip);
        }
    }
    Ok(decisions)
}

/// Resolve the Snap Store name for a config, mirroring
/// [`crate::generate::generate_snap_yaml`]: explicit `name`, else the project
/// name, else the primary binary. Used to address the `list-revisions` probe.
fn resolve_snap_name(
    snap_cfg: &anodizer_core::config::SnapcraftConfig,
    project_name: &str,
    primary_binary: &str,
) -> String {
    snap_cfg.name.clone().unwrap_or_else(|| {
        if project_name.is_empty() {
            primary_binary.to_string()
        } else {
            project_name.to_string()
        }
    })
}

/// Idempotency probe for a snap version: does a revision for `version`
/// already exist, and if so, which of `channels` is it NOT yet released to?
///
/// Runs `snapcraft list-revisions <name>` and parses its tabular output:
/// - `None`                    → either no revision exists for `version` yet
///   (genuine first upload) or the probe itself failed (snapcraft missing,
///   not logged in, snap not registered, network error) → proceed to
///   upload. Never falsely skip a genuine first publish just because the
///   existence check couldn't run; a true auth/network problem resurfaces
///   from the upload itself.
/// - `Some((revision, missing))` where `missing` is empty → the revision
///   already occupies every requested channel — a true re-run at an
///   already-published version → skip (re-uploading would only mint a
///   duplicate Snap Store revision, or hit the content-dedup rejection).
/// - `Some((revision, missing))` where `missing` is non-empty → the bytes
///   were already uploaded (by this run's own earlier attempt, or a prior
///   interrupted run) but never released to one or more configured
///   channels — an orphaned revision. The caller must promote it into
///   `missing` rather than skip (which would silently leave it unpublished)
///   or re-upload (which would only hit the Store's content-dedup
///   rejection).
fn revision_missing_channels(
    snap_name: &str,
    version: &str,
    arch: &str,
    channels: &[String],
    log: &StageLogger,
) -> Option<(String, Vec<String>)> {
    let args = snapcraft_list_revisions_command(snap_name);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    let output = match run_capture_timeout(
        &mut cmd,
        log,
        "snapcraft list-revisions",
        SNAPCRAFT_PROBE_TIMEOUT,
    ) {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // Non-zero exit: snap not registered, not logged in, etc. Can't
            // prove the revision exists, so let the upload proceed.
            log.debug(&format!(
                "snapcraft list-revisions for '{}' exited non-zero ({}); proceeding with upload",
                snap_name, o.status
            ));
            return None;
        }
        Err(e) => {
            // Spawn failure or a deadline kill (probe stalled). Either way the
            // existence check couldn't run — proceed to upload rather than
            // falsely skip; a real auth/network fault resurfaces (bounded) from
            // the upload itself.
            log.debug(&format!(
                "snapcraft list-revisions for '{}' could not run ({}); proceeding with upload",
                snap_name, e
            ));
            return None;
        }
    };
    let combined = log.redact(&String::from_utf8_lossy(&output.stdout));
    let (revision, missing) = missing_channels_for_version(&combined, version, arch, channels)?;
    Some((
        revision,
        missing.into_iter().map(|s| s.to_string()).collect(),
    ))
}

/// After a content-dedup upload rejection, re-query `snapcraft
/// list-revisions` for the revision whose `Version` column equals `version`
/// — the revision that collided with the bytes we just tried to upload.
///
/// A dedup rejection at the SAME version most commonly means an earlier
/// attempt (this run's own retry loop, or a prior failed run) already
/// landed those exact bytes server-side as an orphaned, unreleased
/// revision — the client observed a transient failure (e.g. a 5xx) and
/// retried, but the Store had already ingested the upload. Naming that
/// revision here is what lets the caller promote it instead of reporting a
/// permanent "repack" error for content that, from the operator's
/// perspective, never actually changed.
///
/// `arch` scopes the match to the artifact's own architecture-specific
/// revision — a dual-arch snap mints one revision per arch per version, so
/// matching on `version` alone could name a different arch's revision and
/// have the caller promote (or skip re-uploading) the wrong artifact.
///
/// Returns `None` when the probe itself fails (mirrors
/// [`revision_missing_channels`]'s fail-open stance) or when no revision
/// matches both `version` and `arch` — the latter means the collision is
/// against a DIFFERENT version's (or a different arch's) bytes, so there is
/// nothing to promote.
fn find_colliding_revision(
    snap_name: &str,
    version: &str,
    arch: &str,
    log: &StageLogger,
) -> Option<String> {
    let args = snapcraft_list_revisions_command(snap_name);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    let output = run_capture_timeout(
        &mut cmd,
        log,
        "snapcraft list-revisions",
        SNAPCRAFT_PROBE_TIMEOUT,
    )
    .ok()?;
    if !output.status.success() {
        log.debug(&format!(
            "snapcraft list-revisions for '{snap_name}' exited non-zero while looking for a \
             dedup-colliding revision; cannot identify it"
        ));
        return None;
    }
    let combined = log.redact(&String::from_utf8_lossy(&output.stdout));
    snap_revision_for_version(&combined, version, Some(arch))
}

/// Promote `revision` into every channel in `channels` via `snapcraft
/// release`, stopping at the first failure. Used to recover a dedup
/// rejection whose colliding revision matches the version being published
/// — the bytes already landed, they just were never released.
fn promote_revision(
    snap_name: &str,
    revision: &str,
    channels: &[String],
    log: &StageLogger,
) -> Result<()> {
    for channel in channels {
        let args = snapcraft_release_command(snap_name, revision, channel);
        log.verbose(&format!("running {}", args.join(" ")));
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]);
        let output = run_capture_timeout(
            &mut cmd,
            log,
            "snapcraft release",
            SNAPCRAFT_UPLOAD_TIMEOUT,
        )
        .with_context(|| {
            format!("execute snapcraft release for '{snap_name}' revision {revision} -> {channel}")
        })?;
        log.check_output(output, "snapcraft release")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Upload loop, extracted from `Stage::run` so the
// (attempted, exec_result) seam is testable in isolation.
// ---------------------------------------------------------------------------

/// Run the snapcraft upload loop and report
/// `(attempted_any_upload, Result<()>)`.
///
/// `attempted_any_upload` becomes `true` the first time we materialize a
/// non-dry-run upload (i.e. shell out to `snapcraft upload`). Callers
/// use it to decide whether to record a `PublisherResult`:
/// - `attempted = false` → nothing to report (BlobStage parity); the
///   `Result<()>` is bubbled as a stage error.
/// - `attempted = true`  → fold the `Result<()>` into a
///   `Succeeded`/`Failed(_)` outcome.
///
/// `skip_decisions` is the pre-pass `publish.skip` flag per
/// `(crate, snap_cfg)` tuple in iteration order — keeps this loop free
/// of template-render side effects.
fn run_uploads(
    ctx: &Context,
    crates: &[CrateConfig],
    snap_artifacts: &[Artifact],
    skip_decisions: &[bool],
    log: &StageLogger,
    targets: &mut [SnapcraftTarget],
) -> SnapUploadOutcome {
    let dry_run = ctx.options.dry_run;
    // Wrap snapcraft upload in retry.
    // commit eb944f9 (`isRetriableSnapPush`): 5xx Store responses
    // (500/502/503/504) are transient, every other failure is fatal.
    let retry_policy = ctx.retry_policy();
    let retry_policy = &retry_policy;
    let mut attempted_upload = false;
    let mut skipped_already_published = 0usize;
    let mut held_for_review: Vec<String> = Vec::new();
    let version = ctx.version();
    let project_name = ctx.config.project_name.clone();
    // IIFE captures `attempted_upload` by mutable reference so the
    // `?`-early-exit on per-target failure preserves the "anything
    // attempted before this point?" answer for the caller.
    let result: Result<()> = (|| -> Result<()> {
        let mut decision_idx = 0usize;
        for krate in crates {
            let Some(snap_configs) = krate.snapcrafts.as_ref() else {
                continue;
            };

            for snap_cfg in snap_configs {
                // Pull the pre-rendered skip flag in parallel with the
                // iteration — every `(crate, snap_cfg)` tuple contributes
                // exactly one entry to `skip_decisions`.
                let cfg_skip = skip_decisions.get(decision_idx).copied().unwrap_or(false);
                decision_idx += 1;

                // Only publish configs that opt in
                if !snap_cfg.publish.unwrap_or(false) {
                    continue;
                }
                if cfg_skip {
                    continue;
                }

                // Find snap artifacts for this crate (optionally filtered by id)
                let matching: Vec<&Artifact> = snap_artifacts
                    .iter()
                    .filter(|a| a.crate_name == krate.name)
                    .filter(|a| {
                        if let Some(ref filter_id) = snap_cfg.id {
                            a.metadata
                                .get("id")
                                .map(|id| id == filter_id)
                                .unwrap_or(false)
                        } else {
                            true
                        }
                    })
                    .collect();

                for artifact in &matching {
                    let snap_path = artifact.path.to_string_lossy();
                    // Scopes the idempotency/dedup probes below to this
                    // artifact's own architecture — a dual-arch snap config
                    // (`crates:` targeting both x86_64 and aarch64) mints one
                    // `list-revisions` row per arch per version, and matching
                    // on version alone would let one arch's already-released
                    // revision falsely skip a sibling arch's upload.
                    let arch = triple_to_snap_arch(artifact.target.as_deref().unwrap_or(""));

                    // Each channel template is rendered through the template
                    // engine, dropping only empty results. A render error
                    // propagates rather than silently vanishing a channel from
                    // the --release set (which would mis-route the upload).
                    let rendered_channels: Option<Vec<String>> =
                        match snap_cfg.channel_templates.as_ref() {
                            Some(templates) => {
                                let mut out = Vec::new();
                                for tmpl in templates {
                                    let rendered =
                                        ctx.render_template(tmpl).with_context(|| {
                                            format!("snapcraft: render channel template '{tmpl}'")
                                        })?;
                                    if !rendered.is_empty() {
                                        out.push(rendered);
                                    }
                                }
                                Some(out)
                            }
                            None => None,
                        };
                    // grade is also rendered through the template engine; a
                    // malformed grade must fail, not feed a raw `{{...}}` string
                    // into channel resolution.
                    let rendered_grade: Option<String> = snap_cfg
                        .grade
                        .as_deref()
                        .map(|g| {
                            ctx.render_template(g)
                                .with_context(|| format!("snapcraft: render grade template '{g}'"))
                        })
                        .transpose()?;
                    let effective_channels = resolve_effective_channels(
                        rendered_channels.as_deref(),
                        rendered_grade.as_deref(),
                        snap_cfg.confinement.as_deref(),
                    );

                    // Re-check the RENDERED channels/grade against the same
                    // pre-release restriction the build stage validates on
                    // the raw, unrendered `channel_templates`. A template
                    // that resolves to a forbidden channel only at publish
                    // time (or a `--publish-only` run, which never executes
                    // the build stage's preflight at all) must still be
                    // caught here, before the Snap Store ever sees the
                    // upload.
                    let confinement_is_devmode = snap_cfg.confinement.as_deref() == Some("devmode");
                    let grade_is_devel = rendered_grade.as_deref() == Some("devel");
                    if let Some(channels) = effective_channels.as_deref()
                        && (confinement_is_devmode || grade_is_devel)
                        && let Some(rejected) = first_channel_rejected_for_prerelease_snap(channels)
                    {
                        let reason = match (confinement_is_devmode, grade_is_devel) {
                            (true, true) => "devmode confinement and devel grade",
                            (true, false) => "devmode confinement",
                            (false, true) => "devel grade",
                            (false, false) => unreachable!("guarded by the outer if"),
                        };
                        anyhow::bail!(
                            "snapcraft: crate '{}' configures {reason} together with channel \
                             '{rejected}', which the Snap Store rejects — a snap with {reason} \
                             may only be pushed to pre-release channels (edge, beta). Remove \
                             '{rejected}' from channel_templates or drop the setting that \
                             produces {reason}.",
                            krate.name
                        );
                    }

                    let upload_args =
                        snapcraft_upload_command(&snap_path, effective_channels.as_deref());

                    if dry_run {
                        log.status(&format!("(dry-run) would run: {}", upload_args.join(" "),));
                        continue;
                    }

                    // Idempotency probe: the Snap Store mints a fresh revision
                    // on every upload, so re-running a release at an
                    // already-published version would create a duplicate
                    // revision (and, since the Store dedups on content, the
                    // re-upload would only hit a content-dedup rejection
                    // anyway). A revision uploaded but never released to a
                    // configured channel — orphaned by an earlier
                    // interrupted run — must be promoted, not silently
                    // reported as already published while nothing is
                    // actually live in that channel.
                    let snap_name = resolve_snap_name(
                        snap_cfg,
                        &project_name,
                        &crate::targets::crate_primary_binary(krate),
                    );
                    let probe_channels = effective_channels.clone().unwrap_or_default();
                    match revision_missing_channels(
                        &snap_name,
                        &version,
                        arch,
                        &probe_channels,
                        log,
                    ) {
                        None => {}
                        Some((_, missing)) if missing.is_empty() => {
                            log.status(&format!(
                                "skipped snapcraft '{}' {} — revision already published in the Snap Store",
                                snap_name, version
                            ));
                            skipped_already_published += 1;
                            continue;
                        }
                        Some((revision, missing)) => {
                            attempted_upload = true;
                            promote_revision(&snap_name, &revision, &missing, log).with_context(
                                || {
                                    format!(
                                        "promoting existing snap '{snap_name}' revision {revision} \
                                         to {missing:?} — uploaded by a prior run but never released"
                                    )
                                },
                            )?;
                            log.status(&format!(
                                "promoted snap '{}' {} (revision {}) to {} — recovered an \
                                 already-uploaded revision from a prior run that was never released",
                                snap_name,
                                version,
                                revision,
                                missing.join(", ")
                            ));
                            continue;
                        }
                    }

                    attempted_upload = true;
                    // Set from inside the retry closure when the store answers
                    // with a manual-review hold (Cell because the closure is
                    // re-entered per attempt while the outer scope still reads
                    // the flag afterwards).
                    let review_hold = std::cell::Cell::new(false);
                    // Set from inside the retry closure when a content-dedup
                    // rejection was recovered via promotion rather than a
                    // fresh upload — the final default status line below
                    // must say "promoted", not "uploaded", or the operator
                    // reads a fabricated upload that never happened.
                    let promoted = std::cell::Cell::new(false);
                    // A snapcraft push is idempotent for retry purposes: the
                    // revision-already-published probe above skips a re-run, so
                    // re-issuing the upload after a transient 5xx/network blip
                    // is safe. Keep a transient-error retry floor even when a
                    // stateful mode (`--publish-only`) resolves `attempts: 1`,
                    // mirroring the HTTP-upload, blob, and GitHub asset floors.
                    let upload_policy = retry_policy.with_idempotent_floor();
                    let max_attempts = upload_policy.max_attempts;
                    let retry_desc = format!("snapcraft upload '{snap_name}'");
                    retry_sync_deadline(
                        RetryLog::new(&retry_desc, log),
                        &upload_policy,
                        ctx.retry_deadline(),
                        |attempt| {
                            // Start-of-attempt marker at default visibility: the
                            // upload is a captured subprocess that can run for many
                            // minutes with no other output, so without this line the
                            // stage looks hung until the first attempt ENDS.
                            log.status(&format!(
                                "uploading snap '{}' to the Snap Store (attempt {}/{})",
                                snap_name, attempt, max_attempts
                            )); // status-ok: per-attempt start of a multi-minute captured upload
                            log.verbose(&format!("running {}", upload_args.join(" ")));
                            let mut cmd = Command::new(&upload_args[0]);
                            cmd.args(&upload_args[1..]);
                            let upload_output = match run_capture_timeout(
                                &mut cmd,
                                log,
                                "snapcraft upload",
                                SNAPCRAFT_UPLOAD_TIMEOUT,
                            ) {
                                Ok(o) => o,
                                Err(e) => {
                                    let e = e.context(format!(
                                        "execute snapcraft upload for crate {} snap {}",
                                        krate.name, snap_path
                                    ));
                                    // A deadline kill (upload stalled) is wrapped
                                    // Retriable → retry within budget rather than
                                    // hang forever. A spawn failure (binary missing,
                                    // permission denied) is not transient → break
                                    // without burning the retry budget.
                                    if is_retriable(e.as_ref()) {
                                        return Err(ControlFlow::Continue(e));
                                    }
                                    return Err(ControlFlow::Break(e));
                                }
                            };

                            if upload_output.status.success() {
                                return Ok(());
                            }

                            // Review-pending responses from the Snap Store should be
                            // warnings, not fatal errors — the snap was uploaded
                            // successfully but needs human review. Only the two
                            // genuine review markers flip the hold flag;
                            // "Waiting for previous upload" is a transient
                            // store-processing conflict with no review queued,
                            // so stamping it as HELD would send the operator to
                            // an empty dashboard review queue (the landing
                            // check still reports it honestly if the version
                            // never goes live).
                            const REVIEW_HOLD_STRINGS: &[&str] =
                                &["A human will soon review your snap", "(NEEDS REVIEW)"];
                            const REVIEW_PENDING_STRINGS: &[&str] = &[
                                "Waiting for previous upload",
                                "A human will soon review your snap",
                                "(NEEDS REVIEW)",
                            ];
                            // Redact before any warn-logging — `log.warn` ships
                            // straight to the operator, whereas `log.check_output`
                            // (called below on the fall-through path) already
                            // redacts internally. Without this hop the
                            // review-pending branch would leak any auth tokens
                            // snapcraft happens to echo on stderr.
                            let combined = log.redact(&format!(
                                "{}{}",
                                String::from_utf8_lossy(&upload_output.stdout),
                                String::from_utf8_lossy(&upload_output.stderr),
                            ));
                            if REVIEW_PENDING_STRINGS.iter().any(|s| combined.contains(s)) {
                                log.warn(&format!(
                                    "snap upload pending review — {}",
                                    combined.trim()
                                ));
                                if REVIEW_HOLD_STRINGS.iter().any(|s| combined.contains(s)) {
                                    review_hold.set(true);
                                }
                                return Ok(());
                            }

                            let err = match log.check_output(upload_output, "snapcraft upload") {
                                Ok(_) => return Ok(()),
                                Err(e) => e,
                            };
                            // The 5xx-retriable classifier is checked FIRST,
                            // ahead of the dedup classifier: the two are not
                            // mutually exclusive. Observed in the wild — the
                            // attempt that returns a 5xx can still have
                            // landed the bytes server-side, so the very next
                            // (automatic) retry gets rejected as a duplicate
                            // of what it already ingested, and its output
                            // carries both markers. Checking dedup first
                            // would permanently fast-fail a purely transient
                            // failure.
                            if is_retriable_snap_push(&combined) {
                                return Err(ControlFlow::Continue(err));
                            }
                            if is_content_dedup_rejection(&combined) {
                                // The Store deduplicates on the uploaded
                                // bytes' content hash, not on the
                                // caller-supplied version string. Two
                                // distinct situations produce this
                                // rejection:
                                //
                                // 1. An EARLIER attempt (this run's own retry
                                //    loop, or a prior failed run) already
                                //    landed these exact bytes server-side as
                                //    an orphaned revision that was never
                                //    released to any channel — the fix is to
                                //    PROMOTE that revision, not repack.
                                // 2. The `.snap` genuinely collides with a
                                //    DIFFERENT already-released version's
                                //    bytes — no revision exists at the
                                //    CURRENT version, so there is nothing to
                                //    promote and the package contents must
                                //    change.
                                //
                                // `snapcraft list-revisions` disambiguates:
                                // a revision whose Version column equals the
                                // version being published names case 1.
                                if let Some(revision) =
                                    find_colliding_revision(&snap_name, &version, arch, log)
                                {
                                    let promote_channels =
                                        effective_channels.clone().unwrap_or_default();
                                    if promote_channels.is_empty() {
                                        return Err(ControlFlow::Break(err.context(format!(
                                            "snap upload rejected: content-identical to \
                                             revision {revision} of {snap_name} {version} \
                                             already in the Snap Store, but no channel is \
                                             configured to promote it into — raw snapcraft \
                                             output: {combined}"
                                        ))));
                                    }
                                    return match promote_revision(
                                        &snap_name,
                                        &revision,
                                        &promote_channels,
                                        log,
                                    ) {
                                        Ok(()) => {
                                            promoted.set(true);
                                            log.status(&format!(
                                                "promoted snap {snap_name} {version} \
                                                 (revision {revision}) to {} — recovered from \
                                                 a content-dedup rejection: an earlier attempt \
                                                 had already landed this version's bytes \
                                                 without releasing them",
                                                promote_channels.join(",")
                                            ));
                                            Ok(())
                                        }
                                        Err(promote_err) => {
                                            Err(ControlFlow::Break(err.context(format!(
                                                "snap upload rejected as content-identical to \
                                                 revision {revision} (already-uploaded bytes \
                                                 for {snap_name} {version}), and promoting \
                                                 that revision to {} also failed: \
                                                 {promote_err:#} — raw snapcraft output: \
                                                 {combined}",
                                                promote_channels.join(",")
                                            ))))
                                        }
                                    };
                                }
                                log.warn(&format!(
                                    "snap {snap_name} {version} rejected by Snap Store: \
                                     content-identical to an already-uploaded revision, but \
                                     `snapcraft list-revisions` found no revision at version \
                                     {version} — the collision is against a DIFFERENT \
                                     already-released version's bytes; retrying will not \
                                     help, the package contents must change"
                                ));
                                return Err(ControlFlow::Break(err.context(format!(
                                    "snap upload rejected: content-identical to an \
                                     already-uploaded revision (Snap Store binary_sha3_384 \
                                     dedup) at a version OTHER than {version} — retry will \
                                     not help, the .snap contents must change — raw \
                                     snapcraft output: {combined}"
                                ))));
                            }
                            // Auth failures, malformed snap, quota errors, etc.
                            // fast-fail without burning retry budget.
                            Err(ControlFlow::Break(
                                err.context(format!("raw snapcraft output: {combined}")),
                            ))
                        },
                    )?;
                    if review_hold.get() {
                        // A held upload is NOT a delivered release: the store
                        // accepted the binary but ships nothing until a human
                        // approves it, and a rejection arrives only by email.
                        // Say so at default visibility instead of the
                        // "uploaded" wording, and record the hold on the
                        // evidence snapshot so verify-release and
                        // `--rollback-only --from-run` see the open state.
                        log.warn(&format!(
                            "snap {snap_name} {version} HELD for Snap Store manual review — \
                             not live in any channel until review approves \
                             (https://dashboard.snapcraft.io/snaps/{snap_name}/)"
                        ));
                        held_for_review.push(format!("{snap_name} {version}"));
                        for t in targets
                            .iter_mut()
                            .filter(|t| t.crate_name == krate.name && t.package_name == snap_name)
                        {
                            t.held_for_review = true;
                        }
                    } else if !promoted.get() {
                        log.status(&format!("uploaded snap {} {}", snap_name, version));
                    }
                }
            }
        }
        Ok(())
    })();

    SnapUploadOutcome {
        attempted: attempted_upload,
        skipped_already_published,
        held_for_review,
        result,
    }
}

/// Result of [`run_uploads`]: whether any upload was attempted, how many snaps
/// were skipped because their version was already published, and the
/// upload-phase result.
struct SnapUploadOutcome {
    attempted: bool,
    skipped_already_published: usize,
    /// `"<snap> <version>"` per upload the store answered with a
    /// manual-review hold — accepted but not live until a human approves.
    held_for_review: Vec<String>,
    result: Result<()>,
}

// ---------------------------------------------------------------------------
// PublisherResult recording
// ---------------------------------------------------------------------------

/// Build the `PublishEvidence` recorded on a successful snapcraft run.
///
/// `primary_ref` points at the first uploaded package's snapcraft.io
/// listing; `extra.snapcraft_targets` carries the full per-target
/// snapshot used by `--rollback-only --from-run` to surface the
/// (package, channel) tuples an operator needs to address manually.
fn build_snapcraft_evidence(targets: &[SnapcraftTarget]) -> PublishEvidence {
    let mut evidence = PublishEvidence::new("snapcraft");
    if let Some(first) = targets.first() {
        evidence.primary_ref = Some(format!("https://snapcraft.io/{}", first.package_name));
    }
    evidence.extra = anodizer_core::PublishEvidenceExtra::Snapcraft(
        anodizer_core::publish_evidence::SnapcraftExtra {
            snapcraft_targets: targets.to_vec(),
        },
    );
    evidence
}

/// Append a `PublisherResult` for the snapcraft stage to
/// `ctx.publish_report`. Initializes the report when `None` (covers
/// `--publish` runs where the regular `PublishStage` was skipped).
/// Snapcraft is a Submitter-group publisher; `required` is the caller's
/// pre-derived [`derive_snapcraft_required`] flag.
///
/// Similar role to `stage-blob::run::record_blob_result`; signature is
/// slightly different — this recorder takes a pre-computed
/// `(evidence, outcome)` pair, while the blob recorder derives both
/// from `(uploaded, &exec_result)`. Different shape is fine; the
/// contract (init `publish_report` if `None`; push one
/// `PublisherResult` with `name="snapcraft"`,
/// `group=PublisherGroup::Submitter`) is identical.
pub(crate) fn record_snapcraft_result(
    ctx: &mut Context,
    evidence: Option<PublishEvidence>,
    outcome: PublisherOutcome,
    required: bool,
) {
    if ctx.publish_report.is_none() {
        ctx.publish_report = Some(PublishReport::default());
    }
    let report = ctx
        .publish_report
        .as_mut()
        .expect("publish_report initialized above");
    report.results.push(PublisherResult {
        name: "snapcraft".to_string(),
        group: PublisherGroup::Submitter,
        required,
        outcome,
        evidence,
    });
}

/// Derive the aggregated `required` flag for the snapcraft stage's
/// `PublisherResult`: `true` iff any selected crate's snapcraft config that
/// actually opts into publishing (`publish: true`) also sets
/// `required: true`. A `publish: false` (build-only) config's `required`
/// setting is inert here — it names an upload that will never be
/// attempted, so it must not escalate an unrelated `publish: true` config
/// in the same crate into required. Mirrors
/// `stage-blob::run::derive_blob_required` — one aggregated outcome per
/// stage, one bit per stage, so the submitter gate and the CLI's
/// required-failures exit-code gate just consult
/// `any_failed(Submitter, required_only=true)` without per-config
/// bookkeeping.
///
/// Unset (`None`) resolves to `false`: `required` only governs whether the
/// pipeline ABORTS on a failed snap upload. `verify-release`'s landing
/// check surfaces an attempted-and-failed upload as an issue regardless of
/// this flag — see `landing::run_landing_checks`.
pub(crate) fn derive_snapcraft_required(ctx: &Context) -> bool {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter_map(|c| c.snapcrafts.as_ref())
        .flat_map(|configs| configs.iter())
        .filter(|cfg| cfg.publish.unwrap_or(false))
        .any(|cfg| cfg.required.unwrap_or(false))
}

#[cfg(test)]
mod publish_stage_tests {
    use super::*;
    use crate::targets::decode_snapcraft_targets;
    use anodizer_core::config::{CrateConfig, SnapcraftConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn snap_crate(name: &str, package_name: Option<&str>, channel: Option<&str>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: package_name.map(|s| s.to_string()),
                publish: Some(true),
                channel_templates: channel.map(|c| vec![c.to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    /// A Linux Snap artifact for `crate_name`, matching what `stage-build`
    /// registers before this stage runs. The `crates.is_empty()` /
    /// `snap_artifacts.is_empty()` early-return checks in `Stage::run`
    /// require BOTH a `publish: true` snapcraft config AND a matching
    /// artifact before any of the gate/upload machinery below them
    /// executes — a test that only sets up the crate config, without this,
    /// exercises the "no work, no record" early-return path instead.
    fn snap_artifact(crate_name: &str) -> anodizer_core::artifact::Artifact {
        anodizer_core::artifact::Artifact {
            kind: anodizer_core::artifact::ArtifactKind::Snap,
            name: String::new(),
            path: std::path::PathBuf::from(format!("/tmp/dist/{crate_name}.snap")),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: crate_name.to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        }
    }

    // ---------------------------------------------------------------
    // Idempotent retry floor — the snapcraft upload is an opaque subprocess
    // (`run_capture_timeout`) with no in-process retry-mock seam, so the
    // strongest feasible proof is that the effective upload policy equals
    // `max(global, IDEMPOTENT_PUT_ATTEMPTS)`. This is the same expression the
    // production upload site applies (`retry_policy.with_idempotent_floor()`),
    // so reverting the floor fails this test.
    // ---------------------------------------------------------------

    #[test]
    fn upload_policy_applies_idempotent_floor() {
        use anodizer_core::retry::{IDEMPOTENT_PUT_ATTEMPTS, RetryPolicy};
        use std::time::Duration;

        // A `--publish-only`-shaped policy (attempts: 1) must be raised to the
        // shared idempotent floor so a transient 5xx still retries.
        let capped = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        };
        assert_eq!(
            capped.with_idempotent_floor().max_attempts,
            IDEMPOTENT_PUT_ATTEMPTS,
            "a single-attempt snapcraft policy must be floored to the idempotent minimum"
        );

        // An operator-set cap above the floor must be preserved, never lowered.
        let generous = RetryPolicy {
            max_attempts: 9,
            ..capped
        };
        assert_eq!(
            generous.with_idempotent_floor().max_attempts,
            9,
            "an operator cap above the floor must be preserved"
        );
    }

    // ---------------------------------------------------------------
    // build_snapcraft_evidence — pin the success-path wire shape
    // ---------------------------------------------------------------

    #[test]
    fn build_snapcraft_evidence_pins_success_wire_shape() {
        // Success-path evidence is what `--rollback-only --from-run`
        // and any replay consumer reads back. Pin the three load-bearing
        // fields: publisher name, primary_ref pointing at the first
        // package's snapcraft.io listing, and the full per-target
        // snapshot in extra.snapcraft_targets.
        let targets = vec![
            SnapcraftTarget {
                crate_name: "demo".into(),
                package_name: "demo-snap".into(),
                channel: Some("stable".into()),
                revision: None,
                ..Default::default()
            },
            SnapcraftTarget {
                crate_name: "widget".into(),
                package_name: "widget".into(),
                channel: None,
                revision: None,
                ..Default::default()
            },
        ];
        let evidence = build_snapcraft_evidence(&targets);
        assert_eq!(evidence.publisher, "snapcraft");
        assert_eq!(
            evidence.primary_ref.as_deref(),
            Some("https://snapcraft.io/demo-snap")
        );
        let decoded = decode_snapcraft_targets(&evidence.extra);
        assert_eq!(decoded, targets);
    }

    #[test]
    fn build_snapcraft_evidence_handles_empty_targets() {
        // Edge case: success path with no resolved targets — should
        // still produce a well-formed evidence stub with no
        // primary_ref but an empty snapcraft_targets array.
        let evidence = build_snapcraft_evidence(&[]);
        assert_eq!(evidence.publisher, "snapcraft");
        assert!(evidence.primary_ref.is_none());
        assert_eq!(decode_snapcraft_targets(&evidence.extra), Vec::new());
    }

    // ---------------------------------------------------------------
    // PublisherResult recording behavior
    // ---------------------------------------------------------------

    #[test]
    fn snapshot_mode_records_nothing() {
        // BlobStage parity: snapshot-skip leaves publish_report
        // untouched. Recording a `Skipped(Snapshot)` entry would
        // asymmetrically gate `AnnounceGate::AllPublishers` against
        // snapcraft alone if the announce snapshot-skip-first guard
        // is ever relaxed.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .snapshot(true)
            .build();
        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("snapshot path returns Ok");

        let recorded_snap = ctx
            .publish_report()
            .map(|r| r.results.iter().any(|r| r.name == "snapcraft"))
            .unwrap_or(false);
        assert!(
            !recorded_snap,
            "snapshot mode must NOT record a snapcraft PublisherResult"
        );
    }

    #[test]
    fn submitter_gate_records_skipped_gated() {
        // Pre-seed the publish report with a required Assets failure so
        // the Submitter-gate path fires. Assert the stage records
        // `Skipped(SubmitterGated)` (the gate is observable in the
        // report, not just silent).
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .build();
        // Seed a required Assets failure to trip the gate.
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "blob".to_string(),
            group: PublisherGroup::Assets,
            required: true,
            outcome: PublisherOutcome::Failed("simulated upload failure".to_string()),
            evidence: None,
        });
        ctx.publish_report = Some(report);

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("gate path returns Ok");

        let snap_results: Vec<&PublisherResult> = ctx
            .publish_report()
            .expect("report initialized")
            .results
            .iter()
            .filter(|r| r.name == "snapcraft")
            .collect();
        assert_eq!(snap_results.len(), 1);
        let r = snap_results[0];
        assert_eq!(r.group, PublisherGroup::Submitter);
        assert_eq!(
            r.outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        );
        assert!(r.evidence.is_none(), "gated skip records no evidence");
    }

    #[test]
    fn submitter_gate_fires_on_required_cargo_submitter_failure() {
        // v0.8.0 intra-Submitter fix: snapcraft runs as its own Submitter
        // stage AFTER the trait dispatch. A required cargo (Submitter)
        // failure recorded by that dispatch must close the gate here too —
        // before the fix, snapcraft only consulted Assets/Manager and would
        // have pushed against a half-published crates.io release.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .build();
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "cargo".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Failed("crate-b failed after crate-a published".to_string()),
            evidence: None,
        });
        ctx.publish_report = Some(report);

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("gate path returns Ok");

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry present");
        assert_eq!(
            snap.outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated),
            "a required cargo (Submitter) failure must gate snapcraft"
        );
    }

    #[test]
    fn submitter_gate_stays_open_on_optional_upstream_failure() {
        // Continue-on-error preserved: an OPTIONAL upstream failure must NOT
        // gate snapcraft. The stage must not record Skipped(SubmitterGated),
        // and — positively — the gate predicate it consults must report open.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .build();
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "blob".to_string(),
            group: PublisherGroup::Assets,
            required: false,
            outcome: PublisherOutcome::Failed("optional blob boom".to_string()),
            evidence: None,
        });
        ctx.publish_report = Some(report);

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("ungated path returns Ok");

        let gated = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .any(|r| {
                r.name == "snapcraft"
                    && matches!(
                        r.outcome,
                        PublisherOutcome::Skipped(SkipReason::SubmitterGated)
                    )
            });
        assert!(
            !gated,
            "an optional upstream failure must not gate snapcraft (continue-on-error)"
        );
        // Positive proof the gate is open, not merely that no gated row was
        // recorded (which the no-work path would also satisfy).
        assert!(
            !ctx.publish_report()
                .expect("report present")
                .submitter_gate_closed(),
            "an optional upstream failure must leave the submitter gate open"
        );
    }

    // ---------------------------------------------------------------
    // Pre-submitter verify-release gate — the post-publish content check
    // the in-dispatch Submitter loop also consults, run here too because
    // snapcraft executes as its own pipeline stage outside that loop. A
    // release configuring ONLY `snapcraft:` never puts a single publisher
    // through the in-dispatch loop, so its lazy eval never fires without
    // this stage running the check itself.
    // ---------------------------------------------------------------

    #[test]
    #[serial_test::serial(path_env)]
    fn verify_gate_records_skipped_verify_gate_blocked() {
        // No prior publish_report at all — the snapcraft-only-release
        // shape: no trait-dispatched Submitter publisher ever ran, so
        // this stage is the first and only place the gate can fire.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .build();
        ctx.artifacts.add(snap_artifact("demo"));
        assert!(ctx.publish_report.is_none());
        ctx.verify_gate = Some(std::sync::Arc::new(|_ctx| Ok(false)));

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("gate path returns Ok");

        let report = ctx
            .publish_report()
            .expect("gate check initializes the report");
        assert!(report.verify_gate_evaluated);
        assert!(report.verify_gate_blocked);
        let snap = report
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry present");
        assert_eq!(
            snap.outcome,
            PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked)
        );
        assert!(snap.evidence.is_none(), "gated skip records no evidence");
    }

    #[test]
    #[serial_test::serial(path_env)]
    fn verify_gate_stays_open_when_gate_passes() {
        // A crate + matching artifact must be configured — the gate only
        // runs once there is real snapcraft work to do (the "no work, no
        // record" contract skips the gate entirely otherwise). The gate
        // passing means the stage proceeds into run_uploads and actually
        // spawns `snapcraft`, so it needs the same hermetic stub every other
        // upload-reaching test in this file uses.
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;

        let tools = FakeToolDir::new();
        tools.tool("snapcraft").exit(0).install();
        let _path = tools.activate();

        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .build();
        ctx.artifacts.add(snap_artifact("demo"));
        ctx.verify_gate = Some(std::sync::Arc::new(|_ctx| Ok(true)));

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("ungated path returns Ok");

        let report = ctx
            .publish_report()
            .expect("gate check initializes the report");
        assert!(report.verify_gate_evaluated);
        assert!(!report.verify_gate_blocked);
    }

    #[test]
    #[serial_test::serial(path_env)]
    fn no_work_does_not_evaluate_the_verify_gate() {
        // Without a configured crate or a matching artifact, the stage
        // must take the "no work, no record" early-return before ever
        // reaching the verify-release gate — no live GH asset-verification
        // fetch, no phantom `Skipped(VerifyGateBlocked)` entry.
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let invoked_clone = invoked.clone();
        let mut ctx = TestContextBuilder::new().build();
        ctx.verify_gate = Some(std::sync::Arc::new(move |_ctx| {
            invoked_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(false)
        }));

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("no-work path returns Ok");

        assert_eq!(
            invoked.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a release with no snapcraft work must never invoke the verify gate"
        );
        assert!(
            ctx.publish_report.is_none(),
            "no work attempted — publish_report must stay untouched"
        );
    }

    #[test]
    #[serial_test::serial(path_env)]
    fn verify_gate_evaluated_once_when_dispatch_already_ran_it() {
        // Simulates the shared, cross-crate coordination: the in-dispatch
        // Submitter loop already ran the live gate check (e.g. this
        // release also configures `cargo:`) and persisted its verdict.
        // The stage must trust that verdict rather than invoking the gate
        // closure a second time.
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let invoked_clone = invoked.clone();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .build();
        ctx.artifacts.add(snap_artifact("demo"));
        ctx.verify_gate = Some(std::sync::Arc::new(move |_ctx| {
            invoked_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(false)
        }));
        ctx.publish_report = Some(PublishReport {
            verify_gate_evaluated: true,
            verify_gate_blocked: true,
            ..Default::default()
        });

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("gate path returns Ok");

        assert_eq!(
            invoked.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an already-evaluated gate must not be invoked again"
        );
        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry present");
        assert_eq!(
            snap.outcome,
            PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked)
        );
    }

    #[test]
    fn no_configured_crates_records_nothing() {
        // BlobStage parity: when there is no work to attempt, do NOT
        // append a PublisherResult — the slot stays clean so downstream
        // consumers can distinguish "configured-and-skipped" from
        // "never asked to run".
        let mut ctx = TestContextBuilder::new().build();
        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("no-crates path returns Ok");
        assert!(
            ctx.publish_report().is_none()
                || !ctx
                    .publish_report()
                    .unwrap()
                    .results
                    .iter()
                    .any(|r| r.name == "snapcraft"),
            "no snapcraft entry should be recorded when no crates are configured"
        );
    }

    #[test]
    fn dry_run_with_publishable_config_records_nothing() {
        // Mirrors BlobStage's dry-run contract: we log what WOULD run,
        // but no PublisherResult lands because no upload was attempted.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::context::ContextOptions;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let crate_cfg = snap_crate("demo", Some("demo"), Some("edge"));
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("dry-run returns Ok");
        let recorded_snap = ctx
            .publish_report()
            .map(|r| r.results.iter().any(|r| r.name == "snapcraft"))
            .unwrap_or(false);
        assert!(
            !recorded_snap,
            "dry-run path must NOT record a snapcraft PublisherResult"
        );
    }

    // ---------------------------------------------------------------
    // record_snapcraft_result direct seam — Failed(_) entry coverage
    // ---------------------------------------------------------------

    #[test]
    fn record_snapcraft_result_initializes_report_if_missing() {
        // `--publish` subset runs may invoke `SnapcraftPublishStage`
        // before `PublishStage` has populated `ctx.publish_report`.
        // The recorder must initialize the report on first push.
        let mut ctx = TestContextBuilder::new().build();
        assert!(ctx.publish_report.is_none());
        record_snapcraft_result(
            &mut ctx,
            None,
            PublisherOutcome::Failed("simulated upload failure".into()),
            false,
        );
        let report = ctx.publish_report.as_ref().expect("report initialized");
        assert_eq!(report.results.len(), 1);
        let r = &report.results[0];
        assert_eq!(r.name, "snapcraft");
        assert_eq!(r.group, PublisherGroup::Submitter);
        assert!(!r.required);
        assert_eq!(
            r.outcome,
            PublisherOutcome::Failed("simulated upload failure".into())
        );
        assert!(r.evidence.is_none());
    }

    #[test]
    fn record_snapcraft_result_failed_entry_announce_gate_visibility() {
        // Load-bearing invariant: a failed snap upload lands as a
        // `Failed(_)` entry, NOT a stage-error bail. This is the
        // property the announce gate (`AnnounceGate::AllPublishers`)
        // and `--rollback-only --from-run` consumers depend on —
        // without this entry, neither downstream surface knows the
        // snap upload tried and failed.
        let mut ctx = TestContextBuilder::new().build();
        // Pre-seed something innocuous so we also verify we APPEND
        // (don't clobber) any existing results.
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "github-release".to_string(),
            group: PublisherGroup::Assets,
            required: false,
            outcome: PublisherOutcome::Succeeded,
            evidence: None,
        });
        ctx.publish_report = Some(report);

        record_snapcraft_result(
            &mut ctx,
            None,
            PublisherOutcome::Failed("snapcraft: 401 unauthorized".into()),
            false,
        );

        let report = ctx.publish_report.as_ref().expect("report present");
        assert_eq!(report.results.len(), 2, "appended, did not clobber");
        let snap = report
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry present");
        assert_eq!(snap.group, PublisherGroup::Submitter);
        assert!(!snap.required);
        match &snap.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(msg.contains("401"), "preserves the underlying error: {msg}")
            }
            other => panic!("expected Failed(_), got {other:?}"),
        }
    }

    #[test]
    fn record_snapcraft_result_succeeded_carries_evidence() {
        // Symmetric coverage: Succeeded path attaches evidence.
        let mut ctx = TestContextBuilder::new().build();
        let evidence = build_snapcraft_evidence(&[SnapcraftTarget {
            crate_name: "demo".into(),
            package_name: "demo".into(),
            channel: Some("stable".into()),
            revision: None,
            ..Default::default()
        }]);
        record_snapcraft_result(
            &mut ctx,
            Some(evidence.clone()),
            PublisherOutcome::Succeeded,
            false,
        );
        let report = ctx.publish_report.as_ref().expect("report initialized");
        let snap = report
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry present");
        assert_eq!(snap.outcome, PublisherOutcome::Succeeded);
        assert_eq!(snap.evidence.as_ref(), Some(&evidence));
    }

    // ---------------------------------------------------------------
    // derive_snapcraft_required — Finding 1: an unset/false snapcraft
    // `required:` must still let verify-release surface a failed upload,
    // but must NOT abort the pipeline; an opt-in `required: true` must.
    // Exercised across all three config modes.
    // ---------------------------------------------------------------

    #[test]
    fn derive_snapcraft_required_defaults_false_single_crate() {
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                publish: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        assert!(
            !derive_snapcraft_required(&ctx),
            "an unset `required:` must default to false"
        );
    }

    #[test]
    fn derive_snapcraft_required_true_lockstep_workspace() {
        // Lockstep mode: multiple crates under one top-level `crates:` list
        // sharing one workspace version — any one opting in escalates the
        // aggregated stage-level bit.
        let quiet = CrateConfig {
            name: "quiet".to_string(),
            path: ".".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                publish: Some(true),
                required: Some(false),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let loud = CrateConfig {
            name: "loud".to_string(),
            path: ".".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                publish: Some(true),
                required: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![quiet, loud],
            ..Default::default()
        };
        let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        assert!(
            derive_snapcraft_required(&ctx),
            "any crate's `required: true` must escalate the aggregated stage bit"
        );
    }

    #[test]
    fn derive_snapcraft_required_sees_workspace_only_crate() {
        // Per-crate (workspace) mode: `required: true` on a workspace-only
        // crate must still escalate — a `config.crates`-only derivation
        // would silently miss it.
        let config = anodizer_core::config::Config {
            workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![CrateConfig {
                    name: "ws-only".to_string(),
                    path: ".".to_string(),
                    snapcrafts: Some(vec![SnapcraftConfig {
                        publish: Some(true),
                        required: Some(true),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        assert!(
            ctx.config.crates.is_empty(),
            "fixture must be a pure-workspace config"
        );
        assert!(
            derive_snapcraft_required(&ctx),
            "workspace-only `required: true` must escalate the stage gate"
        );
    }

    #[test]
    fn derive_snapcraft_required_respects_selected_crates_filter() {
        let quiet = CrateConfig {
            name: "quiet".to_string(),
            path: ".".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                publish: Some(true),
                required: Some(false),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let loud = CrateConfig {
            name: "loud".to_string(),
            path: ".".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                publish: Some(true),
                required: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![quiet, loud],
            ..Default::default()
        };
        let options = anodizer_core::context::ContextOptions {
            selected_crates: vec!["quiet".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(config, options);
        assert!(
            !derive_snapcraft_required(&ctx),
            "a `--crate quiet` selection must not see the deselected crate's `required: true`"
        );
    }

    #[test]
    fn derive_snapcraft_required_ignores_build_only_config() {
        // A `publish: false` config's `required: true` names an upload that
        // will never be attempted — it must not escalate an unrelated
        // `publish: true` config in the same crate into required.
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            snapcrafts: Some(vec![
                SnapcraftConfig {
                    publish: Some(false),
                    required: Some(true),
                    ..Default::default()
                },
                SnapcraftConfig {
                    publish: Some(true),
                    required: Some(false),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        assert!(
            !derive_snapcraft_required(&ctx),
            "a build-only config's `required: true` is inert; only \
             `publish: true` configs may escalate the aggregated bit"
        );
    }

    // ---------------------------------------------------------------
    // Pre-pass: skip-template render uniformity
    // ---------------------------------------------------------------

    #[test]
    fn skip_template_error_fast_fails_stage_without_recording() {
        // Important #3 invariant: a malformed `publish.skip` template
        // surfaces as a STAGE ERROR (Err(_) from Stage::run), NOT as
        // a `Failed(_)` PublisherResult — and `publish_report` stays
        // untouched. Two operationally-identical config bugs must not
        // produce different pipeline behaviors depending on which
        // crate iterates first.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::StringOrBool;
        use std::collections::HashMap;
        use std::path::PathBuf;

        // `{{ undefined_var }}` references a template variable that
        // is never set, so Tera errors at render time.
        let snap_cfg = SnapcraftConfig {
            name: Some("demo".to_string()),
            publish: Some(true),
            skip: Some(StringOrBool::String(
                "{{ undefined_var_that_will_not_render }}".to_string(),
            )),
            ..Default::default()
        };
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![snap_cfg]),
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        // Need a snap artifact so the stage reaches the pre-pass
        // (early-return on empty snap_artifacts would mask the
        // template error otherwise).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = SnapcraftPublishStage;
        let err = stage
            .run(&mut ctx)
            .expect_err("template-error must surface as stage error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("render publish.skip template"),
            "error preserves the rendering context: {msg}"
        );
        let recorded_snap = ctx
            .publish_report()
            .map(|r| r.results.iter().any(|r| r.name == "snapcraft"))
            .unwrap_or(false);
        assert!(
            !recorded_snap,
            "publish_report must be untouched on stage-error fast-fail"
        );
    }

    /// A snap-configured crate whose `publish.skip` template references an
    /// undefined var — reaching the pre-pass would hard-error. Used as a
    /// non-invocation oracle: a correctly-firing deselect gate returns
    /// `Ok(Deselected)` before the pre-pass, so the render error never
    /// surfaces; a leaked gate hits the error.
    fn render_error_snap_crate(name: &str) -> CrateConfig {
        use anodizer_core::config::StringOrBool;
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: Some(name.to_string()),
                publish: Some(true),
                skip: Some(StringOrBool::String(
                    "{{ undefined_var_that_will_not_render }}".to_string(),
                )),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    fn assert_snapcraft_deselected_not_uploaded(
        crate_names: &[&str],
        opts: anodizer_core::context::ContextOptions,
    ) {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: crate_names
                .iter()
                .map(|n| render_error_snap_crate(n))
                .collect(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Version", "1.0.0");
        // Seed a snap artifact per crate so a leaked gate would reach the
        // pre-pass (the empty-snap_artifacts early-return must not mask the
        // proof).
        for n in crate_names {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Snap,
                name: String::new(),
                path: PathBuf::from(format!("/tmp/dist/{n}_1.0.0_amd64.snap")),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: n.to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("deselected snapcraft must short-circuit to Ok before the upload pre-pass");

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        assert_eq!(
            snap.outcome,
            PublisherOutcome::Skipped(SkipReason::Deselected),
            "deselected snapcraft must record Skipped(Deselected)"
        );
    }

    #[test]
    fn snapcraft_deselected_by_skip_not_uploaded_single_crate() {
        let opts = anodizer_core::context::ContextOptions {
            skip_stages: vec!["snapcraft-publish".to_string()],
            ..Default::default()
        };
        assert_snapcraft_deselected_not_uploaded(&["demo"], opts);
    }

    #[test]
    fn snapcraft_deselected_by_allowlist_not_uploaded_single_crate() {
        let opts = anodizer_core::context::ContextOptions {
            publisher_allowlist: vec!["cargo".to_string()],
            ..Default::default()
        };
        assert_snapcraft_deselected_not_uploaded(&["demo"], opts);
    }

    #[test]
    fn snapcraft_deselected_by_allowlist_not_uploaded_workspace_per_crate() {
        let opts = anodizer_core::context::ContextOptions {
            publisher_allowlist: vec!["cargo".to_string()],
            ..Default::default()
        };
        assert_snapcraft_deselected_not_uploaded(&["core", "cli"], opts);
    }

    #[test]
    fn snapcraft_deselected_skip_wins_over_allowlist() {
        let opts = anodizer_core::context::ContextOptions {
            skip_stages: vec!["snapcraft-publish".to_string()],
            publisher_allowlist: vec!["snapcraft-publish".to_string()],
            ..Default::default()
        };
        assert_snapcraft_deselected_not_uploaded(&["demo"], opts);
    }

    #[test]
    fn snapcraft_in_allowlist_is_not_deselected() {
        // `--publishers snapcraft-publish`: snapcraft IS selected, so the
        // deselect gate must NOT fire — the render-error config then surfaces
        // its error, proving the upload pre-pass WAS entered.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![render_error_snap_crate("demo")],
            ..Default::default()
        };
        let opts = anodizer_core::context::ContextOptions {
            publisher_allowlist: vec!["snapcraft-publish".to_string()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        let err = SnapcraftPublishStage
            .run(&mut ctx)
            .expect_err("selected snapcraft enters the pre-pass and hits the render error");
        assert!(
            format!("{err:#}").contains("render publish.skip template"),
            "{err}"
        );
    }

    #[test]
    fn run_uploads_no_configured_publishers_returns_not_attempted() {
        // White-box test of the (attempted, exec_result) seam:
        // every snap_cfg is `publish: false`, so the loop runs but
        // never flips `attempted_upload`. exec_result is Ok(()).
        let krate = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(false),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![krate.clone()])
            .build();
        let log = ctx.logger("snapcraft-publish");
        let crates = vec![krate];
        let skip_decisions = vec![false];
        let mut targets = Vec::new();
        let outcome = run_uploads(&ctx, &crates, &[], &skip_decisions, &log, &mut targets);
        assert!(!outcome.attempted, "publish:false → no attempted upload");
        assert_eq!(
            outcome.skipped_already_published, 0,
            "no snaps → nothing skipped"
        );
        assert!(outcome.result.is_ok(), "no work done → Ok(())");
    }

    // Drives the real upload path against a stubbed `snapcraft` whose upload
    // answers with the store's manual-review-hold wording: the run must stay
    // green (a hold can still be approved) while the evidence snapshot and
    // the outcome both carry the unresolved hold instead of a silent
    // "uploaded".
    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn review_hold_is_recorded_on_evidence_not_reported_as_uploaded() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"upload\" ]; then\n\
                 echo \"A human will soon review your snap: (NEEDS REVIEW) confinement 'classic' not allowed\"\n\
                 exit 2\nfi\nexit 1\n",
            )
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("a review hold is non-fatal — the stage must return Ok");

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        assert_eq!(snap.outcome, PublisherOutcome::Succeeded);
        let targets = match &snap.evidence.as_ref().expect("evidence").extra {
            anodizer_core::PublishEvidenceExtra::Snapcraft(e) => &e.snapcraft_targets,
            other => panic!("wrong extra variant: {other:?}"),
        };
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].package_name, "demo");
        assert_eq!(targets[0].version.as_deref(), Some("1.0.0"));
        assert!(
            targets[0].held_for_review,
            "the review hold must be stamped on the evidence snapshot"
        );
    }

    // "Waiting for previous upload" is a transient store-processing conflict,
    // not a manual-review hold — it must keep the non-fatal pending-warn
    // treatment WITHOUT stamping held_for_review (no review queue exists for
    // the operator to visit).
    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn previous_upload_conflict_is_not_stamped_as_review_hold() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"upload\" ]; then\n\
                 echo \"Waiting for previous upload to complete\"\n\
                 exit 2\nfi\nexit 1\n",
            )
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("a pending-upload conflict is non-fatal");

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        let targets = match &snap.evidence.as_ref().expect("evidence").extra {
            anodizer_core::PublishEvidenceExtra::Snapcraft(e) => &e.snapcraft_targets,
            other => panic!("wrong extra variant: {other:?}"),
        };
        assert!(
            !targets[0].held_for_review,
            "a processing conflict is not a review hold"
        );
    }

    // A content-identical upload rejection (Snap Store binary_sha3_384
    // dedup) is permanent for the given bytes — retrying resends the same
    // .snap and gets rejected every time. The upload must fast-fail on the
    // first attempt (no wasted retry budget) and the recorded outcome must
    // carry diagnostic text explaining the rejection is content-based, not
    // a transient store error.
    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn content_dedup_rejection_fails_fast_without_retry() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let counter_dir = tempfile::TempDir::new().unwrap();
        let counter_file = counter_dir.path().join("upload_attempts");
        std::fs::write(&counter_file, "").unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {counter}\n\
                 echo \"Error checking upload uniqueness.\"\n\
                 exit 2\nfi\nexit 1\n",
                counter = counter_file.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Per-target upload failures are reported via `PublisherResult`, not
        // the stage's own `Result` — the stage return stays `Ok(())` so
        // announce-gating and the Submitter gate still run (see the comment
        // at the end of `SnapcraftPublishStage::run`).
        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok even when a publisher fails");

        let attempts = std::fs::read_to_string(&counter_file).unwrap();
        assert_eq!(
            attempts.len(),
            1,
            "a content-dedup rejection must fail fast on the first attempt, \
             never retried — retrying resends identical bytes and gets \
             rejected every time (got {} attempts)",
            attempts.len()
        );

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        match &snap.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(
                    msg.contains("content-identical"),
                    "recorded failure must explain the content-dedup mechanism, got: {msg}"
                );
            }
            other => panic!("expected Failed outcome, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Classifier ordering — a co-occurring 5xx must still retry even when
    // the ambiguous dedup marker is also present in the same output.
    // -----------------------------------------------------------------

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn co_occurring_5xx_and_dedup_marker_still_retries() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{HumanDuration, RetryConfig};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;
        use std::time::Duration;

        let counter_dir = tempfile::TempDir::new().unwrap();
        let counter_file = counter_dir.path().join("upload_attempts");
        std::fs::write(&counter_file, "").unwrap();

        // The FIRST upload attempt's combined output carries both a `[503]`
        // marker (retriable) and the ambiguous "Error checking upload
        // uniqueness." marker (dedup, but only ambiguous, not the
        // definitive form) — the shape observed on the failed CI run this
        // fix was diagnosed from. The retriable classifier must win, so
        // the SECOND attempt gets a chance to succeed cleanly.
        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {counter}\n\
                 n=$(wc -c < {counter})\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                 echo \"[503] Service Unavailable — Error checking upload uniqueness.\"\n\
                 exit 2\nfi\n\
                 exit 0\nfi\n\
                 exit 1\n",
                counter = counter_file.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            retry: Some(RetryConfig {
                attempts: 5,
                delay: HumanDuration(Duration::from_millis(1)),
                max_delay: HumanDuration(Duration::from_millis(1)),
                max_elapsed: None,
            }),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok");

        let attempts = std::fs::read_to_string(&counter_file).unwrap();
        assert_eq!(
            attempts.len(),
            2,
            "a co-occurring 5xx must still retry (2 attempts expected: the \
             failing one and the recovering one), got {} attempt(s)",
            attempts.len()
        );

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        assert!(
            matches!(snap.outcome, PublisherOutcome::Succeeded),
            "expected the retry to recover to Succeeded, got: {:?}",
            snap.outcome
        );
    }

    // -----------------------------------------------------------------
    // Dedup-rejection recovery — a matching-version colliding revision is
    // promoted rather than reported as a permanent repack error.
    // -----------------------------------------------------------------

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn dedup_rejection_with_matching_revision_promotes_instead_of_failing() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let upload_counter_dir = tempfile::TempDir::new().unwrap();
        let upload_counter = upload_counter_dir.path().join("upload_attempts");
        std::fs::write(&upload_counter, "").unwrap();
        let lr_counter_dir = tempfile::TempDir::new().unwrap();
        let lr_counter = lr_counter_dir.path().join("list_revisions_calls");
        std::fs::write(&lr_counter, "").unwrap();
        let release_dir = tempfile::TempDir::new().unwrap();
        let release_log = release_dir.path().join("release_calls");
        std::fs::write(&release_log, "").unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 printf 'x' >> {lr}\n\
                 n=$(wc -c < {lr})\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 if [ \"$n\" -gt 1 ]; then\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 fi\n\
                 echo \"1    2024-01-01T00:00:00Z  amd64   0.9.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {up}\n\
                 echo \"Error checking upload uniqueness.\"\n\
                 exit 2\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
                lr = lr_counter.display(),
                up = upload_counter.display(),
                rel = release_log.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok");

        let upload_attempts = std::fs::read_to_string(&upload_counter).unwrap();
        assert_eq!(
            upload_attempts.len(),
            1,
            "recovery via promotion must not retry the upload — the bytes \
             already landed"
        );

        let release_calls = std::fs::read_to_string(&release_log).unwrap();
        assert_eq!(
            release_calls.trim(),
            "demo 7 stable",
            "expected exactly one `snapcraft release` promoting revision 7 \
             to the configured 'stable' channel, got: {release_calls:?}"
        );

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        assert!(
            matches!(snap.outcome, PublisherOutcome::Succeeded),
            "a recovered dedup rejection must record Succeeded, not Failed, \
             got: {:?}",
            snap.outcome
        );
    }

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn dedup_rejection_promotion_failure_is_reported_distinctly() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        // The FIRST `list-revisions` call is the pre-upload idempotency probe
        // (`revision_missing_channels`), which must NOT see a matching
        // revision yet or it would skip the upload before the dedup-rejection
        // / promotion-recovery path this test targets is ever reached. Only
        // the SECOND+ call (from `find_colliding_revision`, after the upload
        // is rejected as a duplicate) reports the matching revision.
        let lr_counter_dir = tempfile::TempDir::new().unwrap();
        let lr_counter = lr_counter_dir.path().join("list_revisions_calls");
        std::fs::write(&lr_counter, "").unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 printf 'x' >> {lr}\n\
                 n=$(wc -c < {lr})\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 if [ \"$n\" -gt 1 ]; then\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 fi\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 echo \"Error checking upload uniqueness.\"\n\
                 exit 2\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"snapcraft release: 403 Forbidden\"\n\
                 exit 1\n\
                 fi\n\
                 exit 1\n",
                lr = lr_counter.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok even when a publisher fails");

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        match &snap.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(
                    msg.contains("revision 7") && msg.contains("promoting"),
                    "a failed promotion must name the colliding revision and \
                     explain that promotion itself failed, got: {msg}"
                );
            }
            other => panic!("expected Failed outcome, got: {other:?}"),
        }
    }

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn preupload_probe_promotes_orphaned_revision_instead_of_skipping() {
        // A revision for this exact version already exists (an earlier run
        // uploaded it) but its Channels column is "-" — never released. The
        // pre-upload idempotency probe must not silently report
        // Skipped(AlreadyPublished) for content that was never actually
        // published to any channel; it must promote the existing revision.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let upload_counter_dir = tempfile::TempDir::new().unwrap();
        let upload_counter = upload_counter_dir.path().join("upload_attempts");
        std::fs::write(&upload_counter, "").unwrap();
        let release_dir = tempfile::TempDir::new().unwrap();
        let release_log = release_dir.path().join("release_calls");
        std::fs::write(&release_log, "").unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {up}\n\
                 exit 1\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
                up = upload_counter.display(),
                rel = release_log.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok");

        let upload_attempts = std::fs::read_to_string(&upload_counter).unwrap();
        assert_eq!(
            upload_attempts.len(),
            0,
            "an orphaned-but-unreleased revision must be promoted, never re-uploaded \
             (re-upload would only hit the Store's content-dedup rejection)"
        );

        let release_calls = std::fs::read_to_string(&release_log).unwrap();
        assert_eq!(
            release_calls.trim(),
            "demo 7 stable",
            "expected exactly one `snapcraft release` promoting the orphaned revision 7 \
             to the configured 'stable' channel, got: {release_calls:?}"
        );

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        assert!(
            matches!(snap.outcome, PublisherOutcome::Succeeded),
            "recovering an orphaned revision must record Succeeded, not Skipped, got: {:?}",
            snap.outcome
        );
    }

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn preupload_probe_fully_released_revision_still_skips_cleanly() {
        // Regression guard: a revision that IS already released to every
        // configured channel is a true re-run at an already-published
        // version — must still skip cleanly, never upload or promote.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let upload_counter_dir = tempfile::TempDir::new().unwrap();
        let upload_counter = upload_counter_dir.path().join("upload_attempts");
        std::fs::write(&upload_counter, "").unwrap();
        let release_dir = tempfile::TempDir::new().unwrap();
        let release_log = release_dir.path().join("release_calls");
        std::fs::write(&release_log, "").unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {up}\n\
                 exit 1\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
                up = upload_counter.display(),
                rel = release_log.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok");

        assert_eq!(
            std::fs::read_to_string(&upload_counter).unwrap().len(),
            0,
            "a fully-released revision must never be re-uploaded"
        );
        assert_eq!(
            std::fs::read_to_string(&release_log).unwrap().trim(),
            "",
            "a fully-released revision must never be re-promoted"
        );

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        assert!(
            matches!(
                snap.outcome,
                PublisherOutcome::Skipped(SkipReason::AlreadyPublished)
            ),
            "a revision already released everywhere configured must still skip cleanly, \
             got: {:?}",
            snap.outcome
        );
    }

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn preupload_promotion_failure_is_reported_as_failed() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 exit 1\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"snapcraft release: 403 Forbidden\"\n\
                 exit 1\n\
                 fi\n\
                 exit 1\n",
            )
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok even when a publisher fails");

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        match &snap.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(
                    msg.contains("revision 7") && msg.contains("promoting"),
                    "a failed promotion of an orphaned revision must name it and \
                     explain that promotion itself failed, got: {msg}"
                );
            }
            other => panic!("expected Failed outcome, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Dual-arch isolation — a dual-arch snap config (`crates:` targeting
    // both x86_64 and aarch64) mints one `list-revisions` row per arch per
    // version; the amd64 and arm64 legs must be probed independently.
    // -----------------------------------------------------------------

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn dual_arch_arm64_not_skipped_when_only_amd64_published() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let upload_log_dir = tempfile::TempDir::new().unwrap();
        let upload_log = upload_log_dir.path().join("upload_calls");
        std::fs::write(&upload_log, "").unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"5    2026-07-01T00:00:00Z  amd64   1.0.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 echo \"$2\" >> {up}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
                up = upload_log.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_arm64.snap"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok");

        let uploaded = std::fs::read_to_string(&upload_log).unwrap();
        assert_eq!(
            uploaded.trim(),
            "/tmp/dist/demo_1.0.0_arm64.snap",
            "matching on version alone would find amd64's already-released \
             revision 5 and wrongly skip arm64 too; arm64 has no revision of \
             its own yet and must be uploaded, while amd64 must NOT be \
             re-uploaded (it is already published), got: {uploaded:?}"
        );

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        assert!(
            matches!(snap.outcome, PublisherOutcome::Succeeded),
            "one arch skipped + one arch uploaded is still an overall \
             success, got: {:?}",
            snap.outcome
        );
    }

    // -----------------------------------------------------------------
    // Dedup rejection against a DIFFERENT version's bytes — no revision
    // exists at the current version, so there is nothing to promote and the
    // upload must fail with a repack-required error rather than silently
    // succeed or promote the wrong revision.
    // -----------------------------------------------------------------

    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn dedup_rejection_against_different_version_reports_repack_error() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let release_dir = tempfile::TempDir::new().unwrap();
        let release_log = release_dir.path().join("release_calls");
        std::fs::write(&release_log, "").unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"3    2024-01-01T00:00:00Z  amd64   0.9.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 echo \"Error checking upload uniqueness.\"\n\
                 exit 2\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
                rel = release_log.display(),
            ))
            .install();
        let _path = tools.activate();

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SnapcraftPublishStage
            .run(&mut ctx)
            .expect("stage return stays Ok even when a publisher fails");

        assert_eq!(
            std::fs::read_to_string(&release_log).unwrap().trim(),
            "",
            "no revision exists at the current version — there is nothing \
             to promote, so `snapcraft release` must never be called"
        );

        let snap = ctx
            .publish_report()
            .expect("report present")
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry recorded")
            .clone();
        match &snap.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(
                    msg.contains("OTHER than") && msg.contains("contents must change"),
                    "the collision is against a different version's bytes — \
                     `find_colliding_revision` found no revision at the \
                     current version, so the error must say so and direct \
                     the operator to repack rather than retry, got: {msg}"
                );
            }
            other => panic!("expected Failed outcome, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Rendered-channel/grade preflight re-check — a template that only
    // resolves to a forbidden channel at render time (or a `--publish-only`
    // run, which never executes the build stage's raw preflight at all)
    // must still be caught before the Snap Store ever sees the upload.
    // -----------------------------------------------------------------

    #[test]
    fn rendered_channel_rejected_even_though_raw_template_is_not_literally_restricted() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: Some(true),
                    confinement: Some("devmode".to_string()),
                    // The raw string is "{{ .Channel }}" — it does not
                    // literally equal a restricted risk word, so a check
                    // against the unrendered template (the build stage's
                    // preflight) would not catch it. Only after rendering
                    // does it become "stable".
                    channel_templates: Some(vec!["{{ .Channel }}".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Channel", "stable");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let err = SnapcraftPublishStage
            .run(&mut ctx)
            .expect_err("a rendered channel the Store rejects for devmode snaps must abort");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("devmode confinement") && msg.contains("'stable'"),
            "expected the rendered-channel rejection to name devmode \
             confinement and the offending channel, got: {msg}"
        );
    }
}
