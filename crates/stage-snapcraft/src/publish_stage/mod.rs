use anyhow::Result;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anodizer_core::{PublisherOutcome, SkipReason};

use crate::targets::collect_snapcraft_targets;

mod result;
mod revision;
mod uploads;

pub(crate) use result::*;
pub(crate) use revision::*;
pub(crate) use uploads::*;

// The upload/probe wall-clock bounds live in `crate::command` so the publish
// path and the promote path share one source of truth (the `super::*` glob
// re-exports them to the sibling upload/revision modules).
pub(crate) use crate::command::{SNAPCRAFT_PROBE_TIMEOUT, SNAPCRAFT_UPLOAD_TIMEOUT};

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

        // Resolve the planned per-config snapshot BEFORE uploading so the
        // upload loop can clone each snap's channel/version base while
        // stamping the per-arch revision it mints. The recorded per-arch
        // entries returned below feed
        // `PublishEvidence::extra.snapcraft_targets` on success so
        // `promote --from-run` / `--rollback-only --from-run` consumers can
        // release every architecture's revision.
        let planned = collect_snapcraft_targets(ctx);

        let SnapUploadOutcome {
            attempted,
            skipped_already_published,
            held_for_review,
            recorded,
            result: exec_result,
        } = run_uploads(
            ctx,
            &crates,
            &snap_artifacts,
            &skip_decisions,
            &log,
            &planned,
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
            .then(|| build_snapcraft_evidence(&recorded));
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

#[cfg(test)]
mod tests;
