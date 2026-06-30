//! Best-effort rollback dispatch.
//!
//! Invoked from `PublishStage::run` after `dispatch()` returns when a
//! rollback trigger fires AND
//! `ctx.options.rollback_mode != Some(RollbackMode::None)`. Two kinds of
//! target are reverted:
//!
//! - every Assets/Manager publisher that successfully published
//!   (`PublisherOutcome::Succeeded`) — reverted via its API delete / PR
//!   close, transitioning the row to `RolledBack`;
//! - a *failed* required Submitter (cargo) that already pushed crates to
//!   crates.io and opts in via
//!   [`Publisher::programmatic_rollback_on_failure`] — its recorded crates
//!   are yanked. The row KEEPS its `Failed` outcome on a successful yank
//!   (the release genuinely failed); only a yank failure moves it to
//!   `RollbackFailed`.
//!
//! Submitter rollback is informational-only for every other publisher, so
//! they are skipped. Each rollback step is independent: a step's failure
//! becomes `RollbackFailed(err)` on its `PublisherResult`, but the next
//! step still runs.

use anodizer_core::context::{Context, RollbackMode};
use anodizer_core::{PublishReport, Publisher, PublisherGroup, PublisherOutcome};

/// Iterate `report.results` and run rollback for each succeeded
/// Assets/Manager publisher. Per-step outcomes update in place:
///
/// - `RolledBack` on `Ok(())`,
/// - `RollbackFailed(err)` on `Err`,
/// - `RollbackSkippedNoScope` when `rollback_scope_needed()` declares a
///   scope and the corresponding env var is unset.
///
/// A `mode` of `RollbackMode::None` is a no-op; the trigger condition in
/// `PublishStage::run` already short-circuits this path before calling
/// in, but the guard here keeps the function honest for direct test
/// invocations.
pub fn run(
    publishers: &[Box<dyn Publisher>],
    report: &mut PublishReport,
    ctx: &mut Context,
    mode: RollbackMode,
) {
    if mode == RollbackMode::None {
        return;
    }

    // Publishers that own a dedicated stage (blob) are absent from the
    // dispatch `publishers` list but still own reversible remote state. Resolve
    // their seeded report rows here so a successful blob upload that must be
    // torn down deletes its mirrored objects instead of being marked
    // `RollbackFailed("publisher not found")`. Built before the report is taken
    // so the immutable `ctx` borrow ends here.
    let aux = crate::registry::rollback_publishers(ctx);
    let find_publisher = |name: &str| {
        publishers
            .iter()
            .chain(aux.iter())
            .find(|p| p.name() == name)
    };

    let log = ctx.logger("publish");

    // Iterate indices so we can mutate result.outcome in place while
    // borrowing publishers immutably.
    let target_indices: Vec<usize> = report
        .results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            // No evidence -> nothing to roll back, for every branch below.
            let evidence = r.evidence.as_ref()?;
            // Standard path: a succeeded Assets/Manager publisher is
            // reverted via its API delete / PR close.
            let asset_or_manager_succeeded =
                matches!(r.group, PublisherGroup::Assets | PublisherGroup::Manager)
                    && matches!(r.outcome, PublisherOutcome::Succeeded);
            // Cargo path: a *failed* required Submitter that already pushed
            // crates to crates.io still has a real programmatic yank to run
            // (see Publisher::programmatic_rollback_on_failure). Submitter
            // rollback is informational-only for every other publisher, so
            // this branch fires only when the publisher opts in for the
            // recorded evidence.
            let failed_submitter_with_rollback = matches!(r.outcome, PublisherOutcome::Failed(_))
                && r.group == PublisherGroup::Submitter
                && find_publisher(&r.name)
                    .is_some_and(|p| p.programmatic_rollback_on_failure(evidence));
            if asset_or_manager_succeeded || failed_submitter_with_rollback {
                Some(i)
            } else {
                None
            }
        })
        .collect();

    if target_indices.is_empty() {
        log.status("no rollback targets recorded");
        return;
    }

    log.status(&format!(
        "dispatching rollback for {} target(s)",
        target_indices.len()
    ));

    let mut rolled_back = 0usize;
    let mut failed = 0usize;
    let mut skipped_no_scope = 0usize;

    for i in target_indices {
        // Clone the data we need so we can mutate `report.results[i].outcome`
        // afterward without overlapping borrows. Evidence is guaranteed
        // present by the target filter above.
        let (name, evidence, current) = {
            let r = &report.results[i];
            (
                r.name.clone(),
                r.evidence
                    .clone()
                    .expect("evidence present per filter above"),
                r.outcome.clone(),
            )
        };

        let (outcome, disposition) = execute_rollback_step(
            &name, &evidence, &current, publishers, &aux, ctx, "rollback",
        );
        match disposition {
            RollbackDisposition::RolledBack => rolled_back += 1,
            // The live summary folds "publisher not found" into `failed` (the
            // pre-unification behavior); the replay path counts it separately.
            RollbackDisposition::Failed | RollbackDisposition::NotFound => failed += 1,
            RollbackDisposition::SkippedNoScope => skipped_no_scope += 1,
            RollbackDisposition::Retained => {}
        }
        report.results[i].outcome = outcome;
    }

    log.status(&format!(
        "rollback complete — {} rolled back, {} failed, {} skipped-no-scope",
        rolled_back, failed, skipped_no_scope,
    ));
}

/// How a single target resolved, so each caller can keep its own summary
/// counters (the live path folds `NotFound` into "failed"; the replay path
/// reports it separately) without re-deriving them from the lossy
/// [`PublisherOutcome`] mapping.
pub(crate) enum RollbackDisposition {
    RolledBack,
    Failed,
    NotFound,
    SkippedNoScope,
    Retained,
}

/// Roll back ONE recorded publisher result and return its new outcome plus a
/// disposition for counting.
///
/// Resolves the publisher by name across the dispatch `publishers` list AND the
/// stage-owned `aux` list (blob, which owns `BlobStage` rather than a dispatch
/// entry), honors `retain_on_rollback`, gates on `rollback_scope_needed`, then
/// invokes [`Publisher::rollback`] and maps the result. Shared by the live
/// ([`run`]) and replay ([`crate::rollback_only`]) paths so publisher
/// resolution, retain-opt-out, scope gating, and the
/// `Failed`-keeps-its-outcome-on-successful-yank rule cannot drift between
/// them. `prefix` labels the scope-unavailable warning (`"rollback"` /
/// `"rollback-only"`).
pub(crate) fn execute_rollback_step(
    name: &str,
    evidence: &anodizer_core::PublishEvidence,
    current: &PublisherOutcome,
    publishers: &[Box<dyn Publisher>],
    aux: &[Box<dyn Publisher>],
    ctx: &mut Context,
    prefix: &str,
) -> (PublisherOutcome, RollbackDisposition) {
    let log = ctx.logger("publish");
    let Some(publisher) = publishers
        .iter()
        .chain(aux.iter())
        .find(|p| p.name() == name)
    else {
        log.warn(&format!(
            "skipped rollback for '{name}' — publisher not in current registry"
        ));
        return (
            PublisherOutcome::RollbackFailed("publisher not found in current registry".into()),
            RollbackDisposition::NotFound,
        );
    };

    // Publisher opted out of rollback — leave its work (and outcome) in place.
    if publisher.retain_on_rollback() {
        log.status(&format!(
            "skipped rollback for '{name}' — retain_on_rollback is set"
        ));
        return (current.clone(), RollbackDisposition::Retained);
    }

    if let Some(label) = publisher.rollback_scope_needed()
        && !crate::scope::scope_available_with_env(label, ctx.env_source())
    {
        log.warn(&crate::scope::warn_scope_unavailable_msg(
            prefix, name, label,
        ));
        return (
            PublisherOutcome::RollbackSkippedNoScope,
            RollbackDisposition::SkippedNoScope,
        );
    }

    // A failed Submitter (cargo) keeps its `Failed` outcome on a SUCCESSFUL
    // yank: the release genuinely failed (crate B never went live) and
    // reporting `RolledBack` would mask that. Only a succeeded-then-reverted
    // Assets/Manager publisher transitions to `RolledBack`. A yank FAILURE
    // transitions to `RollbackFailed` for both — a live artifact we could not
    // pull, the manual-intervention signal.
    let was_failure = matches!(current, PublisherOutcome::Failed(_));
    log.status(&format!("invoking rollback for '{name}'"));
    match publisher.rollback(ctx, evidence) {
        Ok(()) => {
            let outcome = if was_failure {
                current.clone()
            } else {
                PublisherOutcome::RolledBack
            };
            (outcome, RollbackDisposition::RolledBack)
        }
        Err(err) => {
            let msg = format!("{:#}", err);
            log.warn(&format!("rollback for '{name}' failed: {msg}"));
            (
                PublisherOutcome::RollbackFailed(msg),
                RollbackDisposition::Failed,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Scope-availability tests inject a closed `MapEnvSource` on the test
    //! `Context` (read through `scope_available_with_env(ctx.env_source())`)
    //! rather than mutating process env, so the suite is hermetic and runs
    //! fully in parallel — no `serial_test` group is required.
    use super::*;
    use crate::testing::*;
    use anodizer_core::{
        PublishEvidence, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };

    /// Helper to build a [`PublisherResult`] entry with `Succeeded` +
    /// matching `PublishEvidence`, mirroring what `dispatch()` writes
    /// for a successful publisher.
    fn succeeded(name: &str, group: PublisherGroup, required: bool) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group,
            required,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(PublishEvidence::new(name)),
        }
    }

    /// Helper for a failed entry (no evidence, mirrors `dispatch`).
    fn failed(name: &str, group: PublisherGroup, required: bool, msg: &str) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group,
            required,
            outcome: PublisherOutcome::Failed(msg.into()),
            evidence: None,
        }
    }

    #[test]
    fn rollback_runs_for_succeeded_assets_and_manager() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "assets1",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake("mgr1", PublisherGroup::Manager, true, FakeOutcome::Succeed),
            fake(
                "sub1",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("assets1", PublisherGroup::Assets, false));
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        // Submitter - even succeeded should NOT be rolled back.
        report
            .results
            .push(succeeded("sub1", PublisherGroup::Submitter, false));

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
        assert!(matches!(
            report.results[1].outcome,
            PublisherOutcome::RolledBack
        ));
        // Submitter entry must remain Succeeded - rollback should not
        // touch it regardless of mode.
        assert!(matches!(
            report.results[2].outcome,
            PublisherOutcome::Succeeded
        ));
    }

    #[test]
    fn rollback_skips_failed_publishers() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Fail("boom".into()),
        )];
        let mut report = PublishReport::default();
        report
            .results
            .push(failed("mgr1", PublisherGroup::Manager, true, "boom"));

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        match &report.results[0].outcome {
            PublisherOutcome::Failed(msg) => assert!(msg.contains("boom")),
            other => panic!("expected Failed unchanged, got {:?}", other),
        }
    }

    #[test]
    fn rollback_step_failure_does_not_abort_siblings() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake("first", PublisherGroup::Manager, true, FakeOutcome::Succeed),
            fake_with_rollback(
                "middle",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Succeed,
                FakeRollback::Fail("rollback bang".into()),
            ),
            fake("third", PublisherGroup::Manager, true, FakeOutcome::Succeed),
        ];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("first", PublisherGroup::Manager, true));
        report
            .results
            .push(succeeded("middle", PublisherGroup::Manager, true));
        report
            .results
            .push(succeeded("third", PublisherGroup::Manager, true));

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
        match &report.results[1].outcome {
            PublisherOutcome::RollbackFailed(msg) => assert!(msg.contains("rollback bang")),
            other => panic!("expected RollbackFailed for middle, got {:?}", other),
        }
        assert!(matches!(
            report.results[2].outcome,
            PublisherOutcome::RolledBack
        ));
    }

    #[test]
    fn rollback_records_rollback_failed_outcome_per_step() {
        // Same shape as the previous test, but specifically asserts that
        // the `err` string surfaces verbatim in the `RollbackFailed`
        // payload. Kept as its own case to anchor the contract for
        // downstream summary rendering.
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake_with_rollback(
            "explodes",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            FakeRollback::Fail("very specific error text".into()),
        )];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("explodes", PublisherGroup::Manager, true));

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        match &report.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(
                    msg.contains("very specific error text"),
                    "expected err message in outcome, got '{}'",
                    msg
                );
            }
            other => panic!("expected RollbackFailed, got {:?}", other),
        }
    }

    #[test]
    fn rollback_none_mode_skips_entirely() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));

        run(&publishers, &mut report, &mut ctx, RollbackMode::None);

        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::Succeeded
        ));
    }

    #[test]
    fn rollback_skips_when_no_scope_available() {
        let mut ctx = Context::test_fixture();
        // Inject an empty env source so the `FAKE_TOKEN` scope reads as unset
        // through `scope_available_with_env(ctx.env_source())` — no process-env
        // mutation, so the test is hermetic and needs no serial group.
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        let publishers = vec![fake_with_scope(
            "scoped",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            "FAKE_TOKEN write",
        )];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("scoped", PublisherGroup::Manager, true));

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::RollbackSkippedNoScope
        ));
    }

    #[test]
    fn rollback_skips_when_evidence_missing() {
        // A publisher that recorded Succeeded but somehow lacks
        // evidence (defensive: the dispatcher always writes evidence
        // for Succeeded, but the filter guards anyway). Outcome must
        // not flip to RolledBack - the publisher had nothing to roll
        // back.
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake(
            "noevidence",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "noevidence".into(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: PublisherOutcome::Succeeded,
            evidence: None,
        });

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::Succeeded
        ));
    }

    #[test]
    fn rollback_skips_already_skipped_entries() {
        // A Submitter publisher that was Skipped(SubmitterGated) must
        // not have its outcome rewritten by the rollback dispatcher.
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake(
            "sub1",
            PublisherGroup::Submitter,
            false,
            FakeOutcome::Succeed,
        )];
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "sub1".into(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Skipped(SkipReason::SubmitterGated),
            evidence: None,
        });

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        ));
    }

    #[test]
    fn rollback_marks_failed_when_publisher_not_in_registry() {
        // Edge case: report mentions a publisher that the current
        // registry does not include (e.g. a config change between the
        // publish run and a hypothetical replay). The dispatcher
        // surfaces this as RollbackFailed so the operator sees the
        // dropped target.
        let mut ctx = Context::test_fixture();
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("orphan", PublisherGroup::Manager, true));

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        match &report.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(msg.contains("not found in current registry"))
            }
            other => panic!("expected RollbackFailed, got {:?}", other),
        }
    }

    /// Build a `Context` whose config declares a `blobs:` block so
    /// `registry::rollback_publishers` instantiates a `BlobPublisher`.
    fn ctx_with_blob_configured() -> Context {
        use anodizer_core::config::{BlobConfig, CrateConfig};
        use anodizer_core::test_helpers::TestContextBuilder;
        let crate_cfg = CrateConfig {
            name: "app".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: "mirror".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        TestContextBuilder::new().crates(vec![crate_cfg]).build()
    }

    #[test]
    fn blob_row_resolves_via_rollback_publishers_not_marked_not_found() {
        // The blob-before-doors ordering seeds a Succeeded `blob` (Assets) row
        // into the report before rollback runs. `blob` is deliberately absent
        // from the dispatch registry (it owns BlobStage), so without
        // `registry::rollback_publishers` the loop would mark this row
        // RollbackFailed("publisher not found") and orphan the mirrored
        // objects. With blob configured it must resolve and roll back. (The
        // `succeeded` helper's evidence carries no structured blob_targets, so
        // BlobPublisher::rollback takes its hermetic empty-targets warn path —
        // no network — and returns Ok, flipping the row to RolledBack.)
        let mut ctx = ctx_with_blob_configured();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("blob", PublisherGroup::Assets, true));

        // No dispatch publishers: blob must be resolved from the aux list.
        run(&[], &mut report, &mut ctx, RollbackMode::BestEffort);

        assert!(
            matches!(report.results[0].outcome, PublisherOutcome::RolledBack),
            "blob must resolve via rollback_publishers and roll back, got {:?}",
            report.results[0].outcome
        );
    }

    #[test]
    fn blob_row_marked_not_found_when_blob_not_configured() {
        // Symmetry guard: `rollback_publishers` only instantiates a
        // BlobPublisher when blob is configured. With no `blobs:` block a
        // stray `blob` row genuinely has no owner and must surface as
        // RollbackFailed rather than silently passing.
        let mut ctx = Context::test_fixture();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("blob", PublisherGroup::Assets, true));

        run(&[], &mut report, &mut ctx, RollbackMode::BestEffort);

        match &report.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(msg.contains("not found in current registry"))
            }
            other => panic!("expected RollbackFailed, got {:?}", other),
        }
    }

    #[test]
    fn rollback_skips_publisher_with_retain_on_rollback() {
        // A publisher with retain_on_rollback() = true must not have its
        // rollback() invoked, even if it has succeeded. Its outcome must
        // remain Succeeded after the rollback dispatcher runs.
        struct RetainPublisher;

        impl Publisher for RetainPublisher {
            fn name(&self) -> &str {
                "retain-pub"
            }

            fn group(&self) -> PublisherGroup {
                PublisherGroup::Assets
            }

            fn required(&self) -> bool {
                false
            }

            fn skips_on_nightly(&self) -> bool {
                false
            }

            fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
                Ok(PublishEvidence::new("retain-pub"))
            }

            fn rollback(
                &self,
                _ctx: &mut Context,
                _evidence: &PublishEvidence,
            ) -> anyhow::Result<()> {
                panic!("rollback() was called on a publisher with retain_on_rollback=true")
            }

            fn retain_on_rollback(&self) -> bool {
                true
            }
        }

        let mut ctx = Context::test_fixture();
        let publishers: Vec<Box<dyn Publisher>> = vec![Box::new(RetainPublisher)];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("retain-pub", PublisherGroup::Assets, false));

        run(&publishers, &mut report, &mut ctx, RollbackMode::BestEffort);

        // Outcome must remain Succeeded — rollback was skipped.
        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::Succeeded
        ));
    }
}
