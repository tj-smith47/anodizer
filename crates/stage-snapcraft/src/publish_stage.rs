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

use crate::command::{
    is_retriable_snap_push, resolve_effective_channels, snap_revision_exists_in_output,
    snapcraft_list_revisions_command, snapcraft_upload_command,
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
            record_snapcraft_result(ctx, None, PublisherOutcome::Skipped(SkipReason::Deselected));
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
        record_snapcraft_result(ctx, evidence, outcome);
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

/// Tri-state idempotency probe for a snap version, mirroring cargo's
/// `is_already_published` shape.
///
/// Runs `snapcraft list-revisions <name>` and parses its tabular output:
/// - `Some(true)`  → a revision for `version` already exists → skip the upload
///   (re-uploading would mint a duplicate Snap Store revision).
/// - `Some(false)` → the snap is listed but has no revision at `version` →
///   upload.
/// - `None`        → the probe itself failed (snapcraft missing, not logged in,
///   snap not registered, network error) → upload. Never falsely skip a
///   genuine first publish just because the existence check couldn't run; a
///   true auth/network problem will resurface from the upload itself.
fn snap_revision_already_published(
    snap_name: &str,
    version: &str,
    log: &StageLogger,
) -> Option<bool> {
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
    Some(snap_revision_exists_in_output(&combined, version))
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
                    );
                    let upload_args =
                        snapcraft_upload_command(&snap_path, effective_channels.as_deref());

                    if dry_run {
                        log.status(&format!("(dry-run) would run: {}", upload_args.join(" "),));
                        continue;
                    }

                    // Idempotency probe: the Snap Store mints a fresh revision
                    // on every upload, so re-running a release at an
                    // already-published version would create a duplicate
                    // revision. Skip the upload when a revision for this
                    // version already exists. `None` (probe couldn't run) and
                    // `Some(false)` (no such revision) both proceed to upload.
                    let snap_name = resolve_snap_name(
                        snap_cfg,
                        &project_name,
                        &crate::targets::crate_primary_binary(krate),
                    );
                    if let Some(true) = snap_revision_already_published(&snap_name, &version, log) {
                        log.status(&format!(
                            "skipped snapcraft '{}' {} — revision already published in the Snap Store",
                            snap_name, version
                        ));
                        skipped_already_published += 1;
                        continue;
                    }

                    attempted_upload = true;
                    // Set from inside the retry closure when the store answers
                    // with a manual-review hold (Cell because the closure is
                    // re-entered per attempt while the outer scope still reads
                    // the flag afterwards).
                    let review_hold = std::cell::Cell::new(false);
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
                            if is_retriable_snap_push(&combined) {
                                Err(ControlFlow::Continue(err))
                            } else {
                                // Auth failures, malformed snap, quota errors, etc.
                                // fast-fail without burning retry budget.
                                Err(ControlFlow::Break(err))
                            }
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
                    } else {
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
/// Snapcraft is a Submitter-group publisher with `required = false`.
///
/// Similar role to `stage-blob::run::record_blob_result`; signature is
/// slightly different — this recorder takes a pre-computed
/// `(evidence, outcome)` pair, while the blob recorder derives both
/// from `(uploaded, &exec_result)`. Different shape is fine; the
/// contract (init `publish_report` if `None`; push one
/// `PublisherResult` with `name="snapcraft"`,
/// `group=PublisherGroup::Submitter`, `required=false`) is identical.
pub(crate) fn record_snapcraft_result(
    ctx: &mut Context,
    evidence: Option<PublishEvidence>,
    outcome: PublisherOutcome,
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
        required: false,
        outcome,
        evidence,
    });
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
            tag_template: "v{{ .Version }}".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: package_name.map(|s| s.to_string()),
                publish: Some(true),
                channel_templates: channel.map(|c| vec![c.to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
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
            tag_template: "v{{ .Version }}".to_string(),
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
            tag_template: "v{{ .Version }}".to_string(),
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
            tag_template: "v{{ .Version }}".to_string(),
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
                tag_template: "v{{ .Version }}".to_string(),
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
                tag_template: "v{{ .Version }}".to_string(),
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
}
