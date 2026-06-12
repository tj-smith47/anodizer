use anodizer_core::config::AnnounceGate;
use anodizer_core::context::Context;
use anodizer_core::publish_report::{PublishReport, PublisherOutcome, SkipReason};
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result};

use crate::announcers::dispatch_all_announcers;

/// Evaluate the announce-stage gate against the supplied PublishReport.
///
/// Returns `true` when AnnounceStage must skip and `false` when it
/// should proceed. Pure / report-only so unit tests can drive every
/// gate × report combination without touching the stage body.
pub(crate) fn evaluate_gate(report: Option<&PublishReport>, gate: AnnounceGate) -> bool {
    match gate {
        AnnounceGate::None => false,
        AnnounceGate::RequiredPublishers => report.is_some_and(|r| r.required_failures() > 0),
        AnnounceGate::AllPublishers => report.is_some_and(|r| {
            // Only *failure-like* outcomes gate announce. A naive
            // `!Succeeded` rule would treat happy-path pending states
            // (`PendingModeration` from chocolatey, `PendingValidation`
            // from winget) and `Skipped(NotConfigured)` as failures,
            // silently defeating announce on any release that included
            // a moderated publisher.
            //
            // # Exhaustiveness
            //
            // Each variant is named explicitly (no `_` wildcard) so
            // adding a new `PublisherOutcome` variant is a compile
            // error here — the reviewer of the new variant has to
            // decide whether it gates announce. `matches!` itself
            // does NOT enforce exhaustiveness; an explicit `match`
            // does, which is the shape used below.
            r.results.iter().any(|res| match &res.outcome {
                PublisherOutcome::Failed(_)
                | PublisherOutcome::RollbackFailed(_)
                | PublisherOutcome::Skipped(SkipReason::SubmitterGated) => true,
                PublisherOutcome::Succeeded
                | PublisherOutcome::Skipped(SkipReason::NotConfigured)
                | PublisherOutcome::Skipped(SkipReason::Snapshot)
                | PublisherOutcome::Skipped(SkipReason::DryRun)
                | PublisherOutcome::Skipped(SkipReason::Nightly)
                | PublisherOutcome::Skipped(SkipReason::NotApplicable)
                | PublisherOutcome::Skipped(SkipReason::AlreadyPublished)
                | PublisherOutcome::RolledBack
                | PublisherOutcome::RollbackSkippedNoScope
                | PublisherOutcome::PendingModeration
                | PublisherOutcome::PendingValidation
                | PublisherOutcome::PublishedNoRollback => false,
            })
        }),
    }
}

/// The outcome of evaluating the announce stage's three short-circuits.
///
/// A single evaluation drives both the proceed/skip decision and (for
/// [`announce_body`]) the matching side effect — log line and, on the gate
/// path, the `publish_report.announce_gated` mutation. Centralising the
/// branches here is the single source of truth: [`announce_should_run`] (the
/// pre-publish guard's question) and [`announce_body`] (the live path) both
/// derive from it and so cannot drift.
pub(crate) enum AnnounceDecision {
    /// Every short-circuit passed; the stage dispatches announcers.
    Proceed,
    /// `announce.skip` rendered truthy.
    SkipBySkipTemplate,
    /// `announce.if` rendered falsy.
    SkipByIfCondition,
    /// The `gate_on` PublishReport gate fired. Carries the count of
    /// required-publisher failures for the operator-facing log line.
    GatedByReport { required_failures: usize },
}

/// Evaluate the three template/report short-circuits the real announce path
/// applies — `announce.skip`, `announce.if`, and the `gate_on` PublishReport
/// gate — in dispatch order, returning the first that fires (or
/// [`AnnounceDecision::Proceed`]).
///
/// Read-only (never mutates `publish_report.announce_gated`); the caller owns
/// any side effect. Both [`announce_body`] and [`announce_should_run`] derive
/// from this so the live path and the pre-publish guard ask the same question.
pub(crate) fn announce_decision(
    ctx: &mut Context,
    announce: &anodizer_core::config::AnnounceConfig,
) -> Result<AnnounceDecision> {
    if let Some(ref skip_val) = announce.skip {
        let should_skip = skip_val
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| "announce: render skip template")?;
        if should_skip {
            return Ok(AnnounceDecision::SkipBySkipTemplate);
        }
    }

    let proceed = anodizer_core::config::evaluate_if_condition(
        announce.if_condition.as_deref(),
        "announce",
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        return Ok(AnnounceDecision::SkipByIfCondition);
    }

    if evaluate_gate(ctx.publish_report.as_ref(), announce.gate_on) {
        let required_failures = ctx
            .publish_report
            .as_ref()
            .map_or(0, |r| r.required_failures());
        return Ok(AnnounceDecision::GatedByReport { required_failures });
    }

    Ok(AnnounceDecision::Proceed)
}

/// Whether the announce stage would dispatch any announcer for this run.
///
/// Thin bool projection of [`announce_decision`]: `true` ≡
/// [`AnnounceDecision::Proceed`]. Read-only (never mutates
/// `publish_report.announce_gated`) so the pre-publish render guard can ask
/// the same question the live path answers, without side effects, and so
/// never flags announcers a gate would skip.
pub(crate) fn announce_should_run(
    ctx: &mut Context,
    announce: &anodizer_core::config::AnnounceConfig,
) -> Result<bool> {
    Ok(matches!(
        announce_decision(ctx, announce)?,
        AnnounceDecision::Proceed
    ))
}

// ---------------------------------------------------------------------------
// AnnounceStage
// ---------------------------------------------------------------------------

pub struct AnnounceStage;

impl Stage for AnnounceStage {
    fn name(&self) -> &str {
        "announce"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        // `emit_summary` is invoked by `Pipeline::run` (single source
        // of truth), not here. The pipeline-layer call ensures the
        // summary fires even when announce is operator-skipped via
        // `--skip=announce` (the stage's `run` is never invoked in
        // that case). A fallback call here would double-write the
        // file; leaving ownership at the pipeline-level scope-guard
        // keeps the contract single-homed.
        announce_body(self, ctx)
    }
}

/// Body of `AnnounceStage::run` — kept separated from the trait `run`
/// to make the boundary explicit: the trait `run` is "announce body
/// only" while `Pipeline::run` is responsible for `emit_summary`.
fn announce_body(_stage: &AnnounceStage, ctx: &mut Context) -> Result<()> {
    let log = ctx.logger("announce");
    if ctx.skip_in_snapshot(&log, "announce") {
        return Ok(());
    }
    // Every announcer is
    // skipped on nightly runs (a nightly is not a "release the world
    // should hear about"). Stage-level skip — bypasses the per-provider
    // dispatch entirely so a misconfigured webhook can't bypass the gate.
    if ctx.is_nightly() {
        log.status("announce skipped — nightly run (GoReleaser parity)");
        return Ok(());
    }

    // Refresh Artifacts template var so announce templates can iterate artifacts.
    ctx.refresh_artifacts_var();

    let announce = match ctx.config.announce.clone() {
        Some(a) => a,
        None => {
            log.status("no announce config — skipping");
            return Ok(());
        }
    };

    // Single source of truth for the skip/if/gate_on decision (shared with
    // the pre-publish guard via `announce_should_run`). Only the
    // side-effecting bits — the log line and, on the gate path, the
    // `announce_gated` mutation — live here.
    match announce_decision(ctx, &announce)? {
        AnnounceDecision::Proceed => {}
        AnnounceDecision::SkipBySkipTemplate => {
            log.status("announce.skip evaluated to true — skipping");
            return Ok(());
        }
        AnnounceDecision::SkipByIfCondition => {
            log.status("skipped — `if` condition evaluated falsy");
            return Ok(());
        }
        AnnounceDecision::GatedByReport { required_failures } => {
            // PublishReport-driven gate: configured required (or all)
            // publishers didn't succeed. The flag on PublishReport lets the
            // run-summary JSON expose the skip cleanly to CI.
            let gate_on = announce.gate_on;
            log.status(&format!(
                "announce skipped via gate_on={gate_on:?}; publish_report has \
                 {required_failures} required-failure(s)"
            ));
            if let Some(report_mut) = ctx.publish_report.as_mut() {
                report_mut.announce_gated = true;
            }
            return Ok(());
        }
    }

    let mut errors: Vec<String> = vec![];
    let retry_policy = ctx.retry_policy();

    // Per-version sent-marker makes a re-run idempotent: an announcer that
    // already posted for this version is skipped. Only on the live path — a
    // dry-run sends nothing, so it neither consults nor writes the marker.
    let mut sent_marker = (!ctx.is_dry_run()).then(|| {
        crate::sent_marker::AnnounceSentMarker::load(&ctx.config.dist, &ctx.version(), &log)
    });

    dispatch_all_announcers(
        ctx,
        &announce,
        &retry_policy,
        &log,
        &mut errors,
        sent_marker.as_mut(),
    )?;

    if !errors.is_empty() {
        anyhow::bail!("announce errors:\n{}", errors.join("\n"));
    }

    Ok(())
}

/// End-of-pipeline run-summary emitter: write `summary.json` (and honor
/// `--summary-json=<path>`), then print the per-publisher status rows
/// to stderr.
///
/// Always invoked by `Pipeline::run` at the very end (success or
/// failure) — the audit trail is most valuable when partial failures
/// occur, and owning it at the pipeline layer means `--skip=announce`
/// does not silently drop the summary write.
///
/// Best-effort: a `summary.json` write failure logs a warn but never
/// escalates to a pipeline error — the release itself is unaffected
/// by a missing observability artifact.
pub fn emit_summary(ctx: &mut Context) {
    let summary = anodizer_stage_publish::run_summary::RunSummary::from_context(ctx);
    let path = anodizer_stage_publish::run_summary::summary_path(ctx);
    if let Some(path) = path {
        let log = ctx.logger("announce");
        // One level in: no section is open here (the last stage's guard
        // dropped), so without the bump this line would sit two columns
        // left of every section's body bullets.
        let _indent = anodizer_core::log::indent_one_level();
        match anodizer_stage_publish::run_summary::write_summary_json(&summary, &path) {
            Ok(true) => log.status(&format!("wrote {}", path.display())),
            Ok(false) => log.status(&format!(
                "preserved existing {} (this run carries no publisher results)",
                path.display()
            )),
            Err(err) => log.warn(&format!("failed to write {}: {err}", path.display())),
        }
    }
    // Always emit the per-publisher status rows so non-CI runs see the
    // outcome at a glance — kv rows under their own `Summary` section,
    // keys padded to the widest so the value columns align. Tagged
    // `publisher-summary` (not `announce`) so the section header reads
    // `Summary` rather than the announce stage's phrase.
    let log = ctx.logger("publisher-summary");
    let rows = anodizer_stage_publish::run_summary::status_table_rows(&summary);
    let key_width = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let _section = log.group("publisher-summary");
    for (key, value) in &rows {
        log.kv(key, value, key_width);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use anodizer_core::config::{AnnounceConfig, Config};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::publish_report::{
        PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };
    use anodizer_core::stage::Stage;

    fn failed_result(name: &str, group: PublisherGroup, required: bool) -> PublisherResult {
        PublisherResult {
            name: name.to_string(),
            group,
            required,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        }
    }

    fn make_ctx(gate_on: AnnounceGate, report: Option<PublishReport>) -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            announce: Some(AnnounceConfig {
                gate_on,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.publish_report = report;
        ctx
    }

    // ---- helper coverage -----------------------------------------------

    #[test]
    fn evaluate_gate_none_never_skips() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, true));
        assert!(!evaluate_gate(None, AnnounceGate::None));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::None));
    }

    #[test]
    fn evaluate_gate_required_skips_only_on_required_failure() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::RequiredPublishers));

        r.results
            .push(failed_result("q", PublisherGroup::Submitter, true));
        assert!(evaluate_gate(Some(&r), AnnounceGate::RequiredPublishers));
    }

    #[test]
    fn evaluate_gate_all_skips_on_any_failure() {
        // AllPublishers gates on Failed (regardless of required), not
        // on "any non-Succeeded". This test covers the base failure
        // case; the dedicated tests below pin every happy-path-pending
        // / skip-not-configured variant to NOT gate.
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        assert!(evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    // ---- happy-path-pending outcomes must NOT gate announce ----------

    /// Construct a `PublisherResult` with an arbitrary outcome — used by
    /// the variant-specific tests below to exercise outcomes the basic
    /// `failed_result` helper doesn't reach.
    fn result_with_outcome(
        name: &str,
        group: PublisherGroup,
        required: bool,
        outcome: PublisherOutcome,
    ) -> PublisherResult {
        PublisherResult {
            name: name.to_string(),
            group,
            required,
            outcome,
            evidence: None,
        }
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_pending_moderation() {
        // Chocolatey publishers always end on PendingModeration on a
        // first run — gating announce on this would defeat the stage
        // for any release that includes chocolatey.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "choco",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::PendingModeration,
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_pending_validation() {
        // winget publishers always end on PendingValidation while the
        // microsoft/winget-pkgs PR pipeline runs — same rationale as
        // PendingModeration above.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "winget",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::PendingValidation,
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_skipped_not_configured() {
        // "No work to do" is not a failure.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "p",
            PublisherGroup::Manager,
            false,
            PublisherOutcome::Skipped(SkipReason::NotConfigured),
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_skipped_snapshot_or_dry_run() {
        for reason in [SkipReason::Snapshot, SkipReason::DryRun] {
            let mut r = PublishReport::default();
            r.results.push(result_with_outcome(
                "p",
                PublisherGroup::Manager,
                false,
                PublisherOutcome::Skipped(reason),
            ));
            assert!(
                !evaluate_gate(Some(&r), AnnounceGate::AllPublishers),
                "skipped(reason={reason:?}) must not gate announce",
            );
        }
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_rolled_back_or_published_no_rollback() {
        // Both outcomes represent "publisher reached a clean terminal
        // state without escalating to Failed". They are NOT failures.
        for outcome in [
            PublisherOutcome::RolledBack,
            PublisherOutcome::PublishedNoRollback,
            PublisherOutcome::RollbackSkippedNoScope,
        ] {
            let mut r = PublishReport::default();
            r.results.push(result_with_outcome(
                "p",
                PublisherGroup::Manager,
                false,
                outcome.clone(),
            ));
            assert!(
                !evaluate_gate(Some(&r), AnnounceGate::AllPublishers),
                "outcome={outcome:?} must not gate announce",
            );
        }
    }

    #[test]
    fn evaluate_gate_all_gates_on_rollback_failed() {
        // RollbackFailed IS a failure-like outcome — the operator
        // needs to know announce was suppressed pending recovery.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "p",
            PublisherGroup::Manager,
            false,
            PublisherOutcome::RollbackFailed("transient".into()),
        ));
        assert!(evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_gates_on_submitter_gated_skip() {
        // SubmitterGated means a Submitter publisher was held back
        // because an upstream Assets/Manager failure tripped the
        // dispatch-time submitter gate. From announce's perspective
        // this is a failure-like outcome — the release did not reach
        // every configured channel and announcing as-is would be
        // misleading.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "p",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated),
        ));
        assert!(evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_mixed_happy_path_pending_alongside_succeeded() {
        // Realistic post-release report: chocolatey pending, cargo
        // succeeded, krew skipped(not_configured). No gating expected.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "choco",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::PendingModeration,
        ));
        r.results.push(result_with_outcome(
            "cargo",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::Succeeded,
        ));
        r.results.push(result_with_outcome(
            "krew",
            PublisherGroup::Submitter,
            false,
            PublisherOutcome::Skipped(SkipReason::NotConfigured),
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_returns_false_when_report_is_none() {
        // No report ≡ no failures, so announce proceeds under any gate.
        assert!(!evaluate_gate(None, AnnounceGate::RequiredPublishers));
        assert!(!evaluate_gate(None, AnnounceGate::AllPublishers));
        assert!(!evaluate_gate(None, AnnounceGate::None));
    }

    // ---- stage-level coverage ------------------------------------------

    #[test]
    fn announce_runs_when_gate_is_none() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Submitter, true));
        let mut ctx = make_ctx(AnnounceGate::None, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        // Stage proceeded past the gate, so announce_gated must remain false.
        assert!(!ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_skips_when_gate_required_and_required_failure() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Submitter, true));
        let mut ctx = make_ctx(AnnounceGate::RequiredPublishers, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        assert!(ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_runs_when_gate_required_and_only_optional_failed() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        let mut ctx = make_ctx(AnnounceGate::RequiredPublishers, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        assert!(!ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_skips_when_gate_all_and_any_failure() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        let mut ctx = make_ctx(AnnounceGate::AllPublishers, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        assert!(ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_runs_when_report_is_none() {
        // No report on Context (publish stage --skip'd). All three gates
        // resolve to "proceed" because no failures means nothing to gate on.
        for gate in [
            AnnounceGate::RequiredPublishers,
            AnnounceGate::AllPublishers,
            AnnounceGate::None,
        ] {
            let mut ctx = make_ctx(gate, None);
            assert!(AnnounceStage.run(&mut ctx).is_ok(), "gate={gate:?}");
            assert!(ctx.publish_report.is_none(), "gate={gate:?}");
        }
    }

    #[test]
    fn announce_gate_serializes_as_snake_case() {
        let s = serde_json::to_string(&AnnounceGate::RequiredPublishers).expect("serialize");
        assert_eq!(s, "\"required_publishers\"");
        let s = serde_json::to_string(&AnnounceGate::AllPublishers).expect("serialize");
        assert_eq!(s, "\"all_publishers\"");
        let s = serde_json::to_string(&AnnounceGate::None).expect("serialize");
        assert_eq!(s, "\"none\"");
    }

    #[test]
    fn announce_config_default_gate_is_required_publishers() {
        assert_eq!(
            AnnounceConfig::default().gate_on,
            AnnounceGate::RequiredPublishers
        );
    }
}

#[cfg(test)]
mod summary_tests {
    //! End-of-pipeline run-summary emission. Verifies the summary is
    //! produced regardless of how AnnounceStage resolved (no-op skip,
    //! gate fire, etc.) and that a write failure never escalates into a
    //! pipeline error.

    use super::*;
    use anodizer_core::config::{AnnounceConfig, AnnounceGate, Config};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::publish_report::{
        PublishReport, PublisherGroup, PublisherOutcome, PublisherResult,
    };
    use anodizer_core::stage::Stage;
    use anodizer_stage_publish::run_summary::RunSummary;
    use std::fs;

    fn ctx_with(
        opts: ContextOptions,
        announce_cfg: Option<AnnounceConfig>,
        report: Option<PublishReport>,
    ) -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            announce: announce_cfg,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.2.3");
        ctx.publish_report = report;
        ctx
    }

    fn opts_with_summary_path(p: std::path::PathBuf) -> ContextOptions {
        ContextOptions {
            summary_json_path: Some(p),
            ..ContextOptions::default()
        }
    }

    fn parse_summary(p: &std::path::Path) -> RunSummary {
        let text = fs::read_to_string(p).expect("read summary.json");
        serde_json::from_str(&text).expect("parse summary.json")
    }

    // `emit_summary` is invoked at the pipeline layer (see
    // `crates/cli/src/pipeline/mod.rs::Pipeline::run`), not from inside
    // `AnnounceStage::run`. These tests exercise `emit_summary`
    // directly to keep the stage-level contract pinned; the
    // pipeline-layer integration that ensures the call always fires
    // (including under `--skip=announce`) is covered by the
    // integration test
    // `announce_skipped_via_skip_flag_still_emits_summary` in
    // `crates/cli/tests`.

    #[test]
    fn emit_summary_writes_summary_when_path_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        // No announce config — irrelevant to emit_summary; the test
        // exercises the summary-emission contract directly.
        let mut ctx = ctx_with(
            opts_with_summary_path(summary_path.clone()),
            None,
            Some(PublishReport::default()),
        );
        emit_summary(&mut ctx);

        assert!(summary_path.exists(), "summary.json must be written");
        let summary = parse_summary(&summary_path);
        assert_eq!(summary.schema_version, RunSummary::CURRENT_SCHEMA_VERSION);
        assert_eq!(summary.tag, "v1.2.3");
    }

    #[test]
    fn emit_summary_does_not_panic_when_write_fails() {
        // Path points at a directory — `fs::write` will fail with EISDIR.
        // emit_summary must NOT panic (the write is best-effort); the
        // caller (pipeline) treats it as an observability channel, not
        // a release gate. The function returns `()` so there is no
        // outcome to assert beyond "did not panic."
        let tmp = tempfile::tempdir().expect("tempdir");
        let bad_path = tmp.path().to_path_buf(); // existing directory

        let mut ctx = ctx_with(opts_with_summary_path(bad_path), None, None);
        emit_summary(&mut ctx);
        // No panic = pass.
    }

    #[test]
    fn emit_summary_writes_when_gate_would_fire() {
        // Mirrors the original `announce_stage_emits_summary_when_gate_fires`
        // intent: the summary must be emitted even when announce was
        // gated off. Drive `AnnounceStage.run` first (which sets
        // `announce_gated = true` via the gate path), then invoke
        // `emit_summary` — the order the pipeline layer enforces.
        let tmp = tempfile::tempdir().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        // Required failure + gate=required => gate fires inside
        // AnnounceStage::run, which sets announce_gated=true and
        // returns Ok.
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "cargo".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        let mut ctx = ctx_with(
            opts_with_summary_path(summary_path.clone()),
            Some(AnnounceConfig {
                gate_on: AnnounceGate::RequiredPublishers,
                ..Default::default()
            }),
            Some(report),
        );
        AnnounceStage.run(&mut ctx).expect("run");
        emit_summary(&mut ctx);

        assert!(
            summary_path.exists(),
            "summary written even when gate fires"
        );
        let summary = parse_summary(&summary_path);
        assert!(
            summary.announce_gated,
            "announce_gated must be set by the gate-fire path"
        );
        assert_eq!(summary.results.len(), 1);
        assert_eq!(summary.results[0].status, "failed");
    }

    #[test]
    fn emit_summary_defaults_to_dist_run_dir_when_path_unset() {
        // summary_json_path = None on a real (non-snapshot, non-dry-run)
        // release => the summary still lands at the derived
        // `<dist>/run-<id>/summary.json` default. Without git info the
        // run id falls back to "local" (derive_run_id's documented
        // final fallback).
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut ctx = ctx_with(
            ContextOptions::default(),
            None,
            Some(PublishReport::default()),
        );
        ctx.config.dist = tmp.path().to_path_buf();
        emit_summary(&mut ctx);

        let expected = tmp.path().join("run-local").join("summary.json");
        assert!(
            expected.exists(),
            "summary.json must default to <dist>/run-<id>/ when --summary-json is unset"
        );
        let summary = parse_summary(&expected);
        assert_eq!(summary.schema_version, RunSummary::CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn emit_summary_default_path_skipped_for_snapshot_and_dry_run() {
        // Snapshot / dry-run are not real releases; without an explicit
        // --summary-json they must not pollute `dist/run-*/` (mirrors
        // write_report_to_run_dir's gating).
        for (snapshot, dry_run) in [(true, false), (false, true)] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let opts = ContextOptions {
                snapshot,
                dry_run,
                ..ContextOptions::default()
            };
            let mut ctx = ctx_with(opts, None, Some(PublishReport::default()));
            ctx.config.dist = tmp.path().to_path_buf();
            emit_summary(&mut ctx);

            assert!(
                !tmp.path().join("run-local").exists(),
                "snapshot={snapshot} dry_run={dry_run}: no default summary write"
            );
        }
    }

    #[test]
    fn emit_summary_explicit_path_wins_over_default() {
        // An explicit --summary-json writes there (and ONLY there),
        // even in snapshot mode where the default is suppressed.
        let tmp = tempfile::tempdir().expect("tempdir");
        let explicit = tmp.path().join("explicit-summary.json");
        let opts = ContextOptions {
            snapshot: true,
            summary_json_path: Some(explicit.clone()),
            ..ContextOptions::default()
        };
        let mut ctx = ctx_with(opts, None, Some(PublishReport::default()));
        ctx.config.dist = tmp.path().to_path_buf();
        emit_summary(&mut ctx);

        assert!(explicit.exists(), "explicit path must be honored");
        assert!(
            !tmp.path().join("run-local").exists(),
            "default run-dir write must not fire alongside an explicit path"
        );
    }

    #[test]
    fn emit_summary_default_path_writes_empty_summary_for_report_less_pipelines() {
        // A real (non-snapshot) pipeline that fails BEFORE the publish
        // stage — e.g. a release-asset upload error — exits with
        // publish_report = None. It must STILL leave a default summary
        // on disk (tag + empty results + irreversibly_published: false)
        // so CI recovery has machine-readable proof that nothing was
        // published. A run dir holding a prior REAL summary is protected
        // by write_summary_json's preserve guard, not by skipping the
        // write (see emit_summary_after_failed_release_preserves_burn_evidence).
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut ctx = ctx_with(ContextOptions::default(), None, None);
        ctx.config.dist = tmp.path().to_path_buf();
        emit_summary(&mut ctx);

        let expected = tmp.path().join("run-local").join("summary.json");
        assert!(
            expected.exists(),
            "report-less real run must emit a default summary"
        );
        let summary = parse_summary(&expected);
        assert!(
            summary.results.is_empty(),
            "no publisher dispatched -> empty results"
        );
        assert!(
            !summary.irreversibly_published,
            "nothing published -> not irreversible"
        );
    }

    #[test]
    fn emit_summary_after_failed_release_preserves_burn_evidence() {
        // The clobber sequence: a failed release left a summary with a
        // landed Submitter (burn evidence) at the default path; a
        // standalone `announce` for the same tag then runs report-less.
        // The original summary must survive byte-for-byte — the
        // write-side preserve guard (results-bearing summaries are never
        // overwritten by empty ones) protects it.
        use anodizer_core::publish_report::{PublisherGroup, PublisherOutcome, PublisherResult};

        let tmp = tempfile::tempdir().expect("tempdir");

        // The failed release run: cargo (Submitter) landed.
        let mut release_ctx = ctx_with(ContextOptions::default(), None, None);
        release_ctx.config.dist = tmp.path().to_path_buf();
        release_ctx.publish_report = Some(PublishReport {
            submitter_gated: false,
            announce_gated: false,
            results: vec![PublisherResult {
                name: "cargo".to_string(),
                group: PublisherGroup::Submitter,
                required: true,
                outcome: PublisherOutcome::Succeeded,
                evidence: None,
            }],
        });
        emit_summary(&mut release_ctx);
        let path = tmp.path().join("run-local").join("summary.json");
        let original = fs::read_to_string(&path).expect("release summary written");

        // The follow-up announce-only run: same dist, no report — but
        // pass an explicit summary path at the SAME location to prove
        // the write-side guard holds even when the path gate is
        // bypassed.
        let mut announce_ctx = ctx_with(opts_with_summary_path(path.clone()), None, None);
        announce_ctx.config.dist = tmp.path().to_path_buf();
        emit_summary(&mut announce_ctx);

        assert_eq!(
            fs::read_to_string(&path).expect("summary still present"),
            original,
            "announce-after-failed-release must not clobber burn evidence"
        );
        let parsed: RunSummary = serde_json::from_str(&original).expect("parse");
        assert!(parsed.irreversibly_published, "fixture must carry the burn");
    }

    #[test]
    fn emit_summary_writes_when_announce_stage_was_not_called() {
        // Regression: a release that operator-skipped announce entirely
        // (`--skip=announce` in the pipeline) STILL gets a summary
        // write, because emit_summary lives on Pipeline rather than
        // inside AnnounceStage. Model "AnnounceStage.run never
        // invoked" by simply not calling it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        let mut ctx = ctx_with(
            opts_with_summary_path(summary_path.clone()),
            None,
            Some(PublishReport::default()),
        );
        // Do NOT call AnnounceStage.run — simulate `--skip=announce`.
        emit_summary(&mut ctx);

        assert!(
            summary_path.exists(),
            "summary must be written even when AnnounceStage::run was skipped",
        );
    }
}
