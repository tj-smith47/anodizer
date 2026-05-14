//! Group-aware publisher dispatch.
//!
//! [`dispatch`] iterates the registry's publishers in
//! [`crate::registry::group_dispatch_order`] order (Assets → Manager →
//! Submitter), recording a [`PublisherResult`] per publisher in the
//! returned [`PublishReport`]. Before dispatching the Submitter group the
//! submitter gate fires when:
//!
//! 1. `opts.gate_submitter` is `true` (default), AND
//! 2. any **required** Assets or Manager publisher already failed.
//!
//! When gated, every Submitter publisher records
//! `Skipped(SubmitterGated)` instead of running and
//! `report.submitter_gated` is set to `true`. This is the load-bearing
//! protection against the "chocolatey moderation got submitted, then
//! winget validation failed and we can't undo the choco upload" failure
//! mode.
//!
//! `opts.fail_fast` stops iteration at the first publisher failure within
//! the current group; the partial report is still returned via `Ok`.
//! `Err` is reserved for catastrophic non-publisher errors (impossible
//! IO, malformed config); per-publisher failures land in the report.
//!
//! The existing `PublishStage::run` body is unchanged in this module; the
//! new [`dispatch`] path is exercised only by tests until per-publisher
//! migrations land.

use anodizer_core::context::Context;
use anodizer_core::{
    PublishReport, Publisher, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
};

/// Knobs for [`dispatch`].
///
/// `gate_submitter` defaults to `true`: irreversible Submitter
/// publishers are skipped when a required Assets/Manager publisher
/// failed. `fail_fast` defaults to `false`: the dispatcher continues
/// past a failed publisher so the resulting report enumerates every
/// outcome for the release summary.
#[derive(Debug, Clone, Copy)]
pub struct DispatchOptions {
    pub fail_fast: bool,
    pub gate_submitter: bool,
}

impl Default for DispatchOptions {
    fn default() -> Self {
        Self {
            fail_fast: false,
            gate_submitter: true,
        }
    }
}

/// Dispatch publishers in Assets -> Manager -> Submitter order, applying
/// the Submitter gate when a required Assets/Manager publisher failed.
/// Returns Ok(partial-report) on per-publisher failure or fail-fast;
/// Err is reserved for catastrophic non-publisher errors.
pub fn dispatch(
    publishers: &[Box<dyn Publisher>],
    ctx: &mut Context,
    opts: &DispatchOptions,
) -> anyhow::Result<PublishReport> {
    let mut report = PublishReport::default();
    let group_order = crate::registry::group_dispatch_order();

    'outer: for group in group_order {
        // Submitter-gate check: fire only when entering the Submitter
        // group, gating is enabled, and a required publisher from an
        // earlier reversible group failed.
        if group == PublisherGroup::Submitter
            && opts.gate_submitter
            && (report.any_failed(PublisherGroup::Assets, true)
                || report.any_failed(PublisherGroup::Manager, true))
        {
            for p in publishers.iter().filter(|p| p.group() == group) {
                report.results.push(PublisherResult {
                    name: p.name().into(),
                    group,
                    required: p.required(),
                    outcome: PublisherOutcome::Skipped(SkipReason::SubmitterGated),
                    evidence: None,
                });
            }
            report.submitter_gated = true;
            // Skip the inner per-publisher loop; gate already recorded
            // Skipped(SubmitterGated) for every Submitter publisher.
            continue;
        }

        for p in publishers.iter().filter(|p| p.group() == group) {
            let (outcome, evidence) = match p.run(ctx) {
                Ok(evidence) => (PublisherOutcome::Succeeded, Some(evidence)),
                Err(err) => (PublisherOutcome::Failed(err.to_string()), None),
            };
            let failed = matches!(outcome, PublisherOutcome::Failed(_));
            report.results.push(PublisherResult {
                name: p.name().into(),
                group,
                required: p.required(),
                outcome,
                evidence,
            });
            if failed && opts.fail_fast {
                break 'outer;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[test]
    fn empty_registry_yields_empty_report() {
        let mut ctx = Context::test_fixture();
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok for empty input");
        assert!(report.results.is_empty());
        assert!(!report.submitter_gated);
    }

    #[test]
    fn group_order_is_assets_manager_submitter() {
        let mut ctx = Context::test_fixture();
        // Intentionally registered out of order to prove the dispatcher
        // re-orders by group rather than relying on input order.
        let publishers = vec![
            fake(
                "sub",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "assets",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "manager",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        let names: Vec<&str> = report.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["assets", "manager", "sub"]);
        assert!(!report.submitter_gated);
    }

    #[test]
    fn submitter_gate_skips_submitter_when_required_manager_fails() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("manager boom".into()),
            ),
            fake(
                "submitter",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(report.submitter_gated);

        let submitter = report
            .results
            .iter()
            .find(|r| r.name == "submitter")
            .expect("submitter entry present");
        assert!(matches!(
            submitter.outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        ));
        assert!(submitter.evidence.is_none());
    }

    #[test]
    fn submitter_runs_when_only_optional_publishers_fail() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "manager",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("optional manager boom".into()),
            ),
            fake(
                "submitter",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(!report.submitter_gated);

        let submitter = report
            .results
            .iter()
            .find(|r| r.name == "submitter")
            .expect("submitter entry present");
        assert!(matches!(submitter.outcome, PublisherOutcome::Succeeded));
    }

    #[test]
    fn no_gate_submitter_runs_submitter_anyway() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("required manager boom".into()),
            ),
            fake(
                "submitter",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let opts = DispatchOptions {
            fail_fast: false,
            gate_submitter: false,
        };
        let report = dispatch(&publishers, &mut ctx, &opts).expect("dispatch returns Ok");
        assert!(!report.submitter_gated);

        let submitter = report
            .results
            .iter()
            .find(|r| r.name == "submitter")
            .expect("submitter entry present");
        assert!(
            matches!(submitter.outcome, PublisherOutcome::Succeeded),
            "submitter should run when gate_submitter=false even on required-manager failure"
        );
    }

    #[test]
    fn fail_fast_aborts_at_first_error() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake("m1", PublisherGroup::Manager, false, FakeOutcome::Succeed),
            fake(
                "m2",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("boom".into()),
            ),
            fake("m3", PublisherGroup::Manager, false, FakeOutcome::Succeed),
        ];
        let opts = DispatchOptions {
            fail_fast: true,
            gate_submitter: true,
        };
        let report = dispatch(&publishers, &mut ctx, &opts)
            .expect("fail_fast still returns Ok with the partial report");

        let names: Vec<&str> = report.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["m1", "m2"], "m3 must not have run");
        assert!(report.results.iter().all(|r| r.name != "m3"));
    }
}
