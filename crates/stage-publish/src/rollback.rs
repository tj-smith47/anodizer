//! Best-effort rollback dispatch for Assets/Manager publishers.
//!
//! Invoked from `PublishStage::run` after `dispatch()` returns when any
//! required Assets/Manager publisher failed AND
//! `ctx.options.rollback_mode != Some(RollbackMode::None)`. The set of
//! rollback targets is every Assets/Manager publisher that successfully
//! published (`PublisherOutcome::Succeeded`); Submitter publishers and
//! already-failed publishers are skipped. Each rollback step is
//! independent: a step's failure becomes `RollbackFailed(err)` on its
//! `PublisherResult`, but the next step still runs.

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

    let log = ctx.logger("publish");

    // Iterate indices so we can mutate result.outcome in place while
    // borrowing publishers immutably.
    let target_indices: Vec<usize> = report
        .results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            if !matches!(r.group, PublisherGroup::Assets | PublisherGroup::Manager) {
                return None;
            }
            if !matches!(r.outcome, PublisherOutcome::Succeeded) {
                return None;
            }
            // No evidence -> nothing to roll back; defensive guard
            // (the dispatcher always writes evidence for Succeeded).
            r.evidence.as_ref()?;
            Some(i)
        })
        .collect();

    if target_indices.is_empty() {
        log.status("rollback: no Assets/Manager rollback targets recorded");
        return;
    }

    log.status(&format!(
        "rollback: dispatching {} target(s)",
        target_indices.len()
    ));

    let mut rolled_back = 0usize;
    let mut failed = 0usize;
    let mut skipped_no_scope = 0usize;

    for i in target_indices {
        // Clone the data we need to call rollback() so we can later
        // mutate `report.results[i].outcome` without overlapping
        // borrows.
        let (name_owned, evidence_owned) = {
            let r = &report.results[i];
            (
                r.name.clone(),
                r.evidence
                    .clone()
                    .expect("evidence present per filter above"),
            )
        };

        // Find the publisher by name.
        let Some(publisher) = publishers.iter().find(|p| p.name() == name_owned) else {
            log.warn(&format!(
                "rollback: publisher '{}' not in current registry; skipping rollback",
                name_owned,
            ));
            failed += 1;
            report.results[i].outcome =
                PublisherOutcome::RollbackFailed("publisher not found in current registry".into());
            continue;
        };

        // If rollback_scope_needed() returns Some but the scope isn't
        // available, skip with the RollbackSkippedNoScope outcome.
        if let Some(label) = publisher.rollback_scope_needed()
            && !crate::scope::scope_available(label)
        {
            skipped_no_scope += 1;
            report.results[i].outcome = PublisherOutcome::RollbackSkippedNoScope;
            log.warn(&crate::scope::warn_scope_unavailable_msg(
                "rollback",
                &name_owned,
                label,
            ));
            continue;
        }

        log.status(&format!("rollback: invoking '{}'", name_owned));
        match publisher.rollback(ctx, &evidence_owned) {
            Ok(()) => {
                rolled_back += 1;
                report.results[i].outcome = PublisherOutcome::RolledBack;
            }
            Err(err) => {
                failed += 1;
                let msg = format!("{:#}", err);
                report.results[i].outcome = PublisherOutcome::RollbackFailed(msg.clone());
                log.warn(&format!("rollback: '{}' failed: {}", name_owned, msg));
            }
        }
    }

    log.status(&format!(
        "rollback: {} rolled back, {} failed, {} skipped-no-scope",
        rolled_back, failed, skipped_no_scope,
    ));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Env-mutating tests in this module use `serial_test` with the
    //! shared group name `rollback_env`. Any new test that calls
    //! `std::env::set_var` / `remove_var` (directly or through a future
    //! helper) MUST carry `#[serial(rollback_env)]` — without it the
    //! `unsafe` env mutations can race a concurrent reader in another
    //! test, which is UB per the `set_var` contract. The group name
    //! is distinct from `scope_env` (used in `scope.rs::tests`) so
    //! the two suites don't serialize against each other unnecessarily.
    use super::*;
    use crate::testing::*;
    use anodizer_core::{
        PublishEvidence, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };
    use serial_test::serial;

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
    #[serial(rollback_env)]
    fn rollback_skips_when_no_scope_available() {
        // Ensure the env var isn't set by any sibling test that mutated
        // it without cleanup. The `serial(rollback_env)` attribute pins
        // ordering so the value the test reads is the one it wrote.
        // Safe inside the serial-guarded block: no concurrent reader
        // can observe the in-flight mutation.
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            std::env::remove_var("FAKE_TOKEN");
        }

        let mut ctx = Context::test_fixture();
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
}
