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
            // Nightly skip-list: publishers that opt out of `--nightly`
            // record `Skipped(Nightly)` and never invoke `run`. Matches
            // The documented nightlies skip set
            // (homebrew, scoop, aur, krew, nix, all announcers, gomod
            // proxy). Honoured before `simulate_failure` so the test
            // harness still observes the real nightly gate.
            if ctx.is_nightly() && p.skips_on_nightly() {
                report.results.push(PublisherResult {
                    name: p.name().into(),
                    group,
                    required: p.required(),
                    outcome: PublisherOutcome::Skipped(SkipReason::Nightly),
                    evidence: None,
                });
                continue;
            }
            // Test-harness short-circuit: when the operator listed this
            // publisher in `--simulate-failure` (env-gated by
            // `ANODIZE_TEST_HARNESS=1` at the CLI layer), bypass
            // `p.run()` entirely and record a synthetic failure so the
            // gate / rollback / report paths can be exercised
            // deterministically without monkey-patching the publisher.
            let simulated = ctx
                .options
                .simulate_failure_publishers
                .iter()
                .any(|name| name == p.name());
            let (outcome, evidence) = if simulated {
                (
                    PublisherOutcome::Failed(format!("simulated failure: {}", p.name())),
                    None,
                )
            } else {
                // Drain any stale override before invoking `run` so a
                // prior publisher cannot bleed its outcome forward.
                let _ = ctx.take_pending_outcome();
                match p.run(ctx) {
                    Ok(evidence) => {
                        // If `run` recorded an outcome override (e.g.
                        // chocolatey moderation skip or PR-already-exists
                        // skip) use it instead of the default `Succeeded`
                        // mapping so the summary table reflects the real
                        // terminal state.
                        let outcome = ctx
                            .take_pending_outcome()
                            .unwrap_or(PublisherOutcome::Succeeded);
                        (outcome, Some(evidence))
                    }
                    Err(err) => (PublisherOutcome::Failed(format!("{err:#}")), None),
                }
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

    #[test]
    fn dispatch_records_pending_moderation_when_run_recorded_override() {
        // Mirrors chocolatey's in-moderation skip path: `run()` returns
        // `Ok(evidence)` but the publisher's actual terminal state is
        // `PendingModeration` because the push was skipped. Dispatch
        // must surface that on the per-publisher row instead of the
        // default `Succeeded` mapping, or the summary table silently
        // misreports the skip as success.
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake_with_pending_outcome(
            "chocolatey",
            PublisherGroup::Submitter,
            false,
            PublisherOutcome::PendingModeration,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let result = report
            .results
            .iter()
            .find(|r| r.name == "chocolatey")
            .expect("chocolatey entry present");
        assert!(
            matches!(result.outcome, PublisherOutcome::PendingModeration),
            "expected PendingModeration, got {:?}",
            result.outcome
        );
        assert!(
            result.evidence.is_some(),
            "evidence from run() should still be recorded alongside the override"
        );
    }

    #[test]
    fn dispatch_records_pending_validation_when_run_recorded_override() {
        // Mirrors winget/krew/homebrew-cask's PR-already-exists skip
        // path: `run()` returns `Ok(evidence)` but the actual terminal
        // state is `PendingValidation` because the PR could not be
        // created or updated. Dispatch must surface that override.
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake_with_pending_outcome(
            "winget",
            PublisherGroup::Submitter,
            false,
            PublisherOutcome::PendingValidation,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let result = report
            .results
            .iter()
            .find(|r| r.name == "winget")
            .expect("winget entry present");
        assert!(
            matches!(result.outcome, PublisherOutcome::PendingValidation),
            "expected PendingValidation, got {:?}",
            result.outcome
        );
    }

    #[test]
    fn dispatch_records_full_anyhow_chain_on_failure() {
        // The user-visible failure message must surface every wrapped
        // `anyhow::Context` layer, not just the outermost one. Using
        // `Display` (the default `err.to_string()`) drops every cause,
        // which was the root cause of the mcp publisher's HTTP-status-less
        // "publish to https://..." log line — the inner HTTP 422 body was
        // discarded before reaching the operator. The recorded message
        // must contain BOTH the outer and inner segments, exactly as
        // `format!("{err:#}")` produces them.
        //
        // Parametrized across every `PublisherGroup` variant so a future
        // refactor that branches the closure per group cannot regress
        // one path silently. Assets/Manager/Submitter all flow through
        // the same `match p.run(ctx)` arm today — keep them all locked
        // in.
        use anodizer_core::PublishEvidence;
        use anyhow::Context as _;

        struct WrappedFailPublisher {
            name: &'static str,
            group: PublisherGroup,
        }
        impl Publisher for WrappedFailPublisher {
            fn name(&self) -> &str {
                self.name
            }
            fn group(&self) -> PublisherGroup {
                self.group
            }
            fn required(&self) -> bool {
                false
            }
            fn skips_on_nightly(&self) -> bool {
                false
            }
            fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
                Err::<(), _>(anyhow::anyhow!("inner"))
                    .context("middle")
                    .context("outer")?;
                unreachable!()
            }
            fn rollback(
                &self,
                _ctx: &mut Context,
                _evidence: &PublishEvidence,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        for (group, label) in [
            (PublisherGroup::Assets, "assets"),
            (PublisherGroup::Manager, "manager"),
            (PublisherGroup::Submitter, "submitter"),
        ] {
            let mut ctx = Context::test_fixture();
            let publishers: Vec<Box<dyn Publisher>> =
                vec![Box::new(WrappedFailPublisher { name: label, group })];
            let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
                .expect("dispatch returns Ok");

            let entry = report
                .results
                .iter()
                .find(|r| r.name == label)
                .unwrap_or_else(|| panic!("{label} entry present"));
            match &entry.outcome {
                PublisherOutcome::Failed(msg) => {
                    assert!(
                        msg.contains("outer"),
                        "{label}: outermost context must be present, got: {msg}"
                    );
                    assert!(
                        msg.contains("inner"),
                        "{label}: innermost cause must be present (anyhow `{{:#}}` chain), got: {msg}"
                    );
                }
                other => panic!("{label}: expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn dispatch_drains_stale_override_between_publishers() {
        // The override slot is single-shot: an earlier publisher's
        // override must not bleed into a later publisher whose `run`
        // recorded nothing. Without the drain at the top of every
        // `run` invocation, a chocolatey moderation skip would
        // contaminate the next publisher's row.
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake_with_pending_outcome(
                "first",
                PublisherGroup::Manager,
                false,
                PublisherOutcome::PendingModeration,
            ),
            fake(
                "second",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let first = report
            .results
            .iter()
            .find(|r| r.name == "first")
            .expect("first entry present");
        assert!(matches!(first.outcome, PublisherOutcome::PendingModeration));

        let second = report
            .results
            .iter()
            .find(|r| r.name == "second")
            .expect("second entry present");
        assert!(
            matches!(second.outcome, PublisherOutcome::Succeeded),
            "second publisher recorded no override; must default to Succeeded, got {:?}",
            second.outcome
        );
    }

    #[test]
    fn dispatch_substitutes_err_for_simulated_failure() {
        // Even when a publisher's `run()` would succeed, listing it in
        // `ctx.options.simulate_failure_publishers` must short-circuit
        // and record a synthetic `Failed("simulated failure: <name>")`
        // entry. Sibling publishers continue to run normally so the
        // gate/fail-fast/rollback paths can be tested in isolation.
        let mut ctx = Context::test_fixture();
        ctx.options.simulate_failure_publishers = vec!["cargo".to_string()];
        let publishers = vec![
            fake("cargo", PublisherGroup::Manager, true, FakeOutcome::Succeed),
            fake("brew", PublisherGroup::Manager, false, FakeOutcome::Succeed),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let cargo = report
            .results
            .iter()
            .find(|r| r.name == "cargo")
            .expect("cargo entry present");
        match &cargo.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(
                    msg.contains("simulated failure") && msg.contains("cargo"),
                    "simulated-failure message should name the publisher, got: {}",
                    msg
                );
            }
            other => panic!("expected Failed, got {:?}", other),
        }
        assert!(
            cargo.evidence.is_none(),
            "simulated failure has no evidence"
        );

        let brew = report
            .results
            .iter()
            .find(|r| r.name == "brew")
            .expect("brew entry present");
        assert!(
            matches!(brew.outcome, PublisherOutcome::Succeeded),
            "non-simulated sibling publisher must run normally"
        );

        // Required-cargo failed + default gate_submitter => Submitter
        // group (if present) would be gated. Even without submitters
        // here the report should mark `any_failed(Manager, true)`.
        assert!(report.any_failed(PublisherGroup::Manager, true));
    }

    #[test]
    fn dispatch_skips_publisher_marked_skips_on_nightly_when_nightly() {
        let mut ctx = Context::test_fixture();
        ctx.options.nightly = true;
        let publishers = vec![
            fake_with_nightly_skip(
                "skipper",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "runner",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        let skipper = report
            .results
            .iter()
            .find(|r| r.name == "skipper")
            .expect("skipper entry present");
        assert!(
            matches!(
                skipper.outcome,
                PublisherOutcome::Skipped(SkipReason::Nightly)
            ),
            "expected Skipped(Nightly), got {:?}",
            skipper.outcome
        );
        assert!(skipper.evidence.is_none());
        let runner = report
            .results
            .iter()
            .find(|r| r.name == "runner")
            .expect("runner entry present");
        assert!(
            matches!(runner.outcome, PublisherOutcome::Succeeded),
            "runner should not be skipped; got {:?}",
            runner.outcome
        );
    }

    #[test]
    fn dispatch_runs_publisher_marked_skips_on_nightly_when_not_nightly() {
        let mut ctx = Context::test_fixture();
        // options.nightly defaults to false
        let publishers = vec![fake_with_nightly_skip(
            "would_skip_on_nightly",
            PublisherGroup::Manager,
            false,
            FakeOutcome::Succeed,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        let entry = report
            .results
            .iter()
            .find(|r| r.name == "would_skip_on_nightly")
            .expect("entry present");
        assert!(
            matches!(entry.outcome, PublisherOutcome::Succeeded),
            "publisher with skips_on_nightly=true must still run when !is_nightly(); got {:?}",
            entry.outcome
        );
    }
}
