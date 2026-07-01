//! Sibling-publisher isolation test (closes [B10]).
//!
//! Asserts that a failing publisher in the middle of a group does not
//! abort its siblings by aborting the whole dispatch. Isolation is
//! asymmetric across a REQUIRED failure in a one-way-door group: siblings
//! that already dispatched (and every sibling after an OPTIONAL failure)
//! still run, but a sibling AFTER a required failure is gated, never fired
//! against an incomplete release (the one-way-door gate). Guards against
//! future regression of both halves.
//!
//! Drives the canonical [`anodizer_stage_publish::testing::FakePublisher`]
//! (exposed via the `test-support` feature, enabled by this crate's own
//! `[dev-dependencies]`) so the shape of the double — including the
//! trait-default `rollback` — stays in lockstep with the in-crate unit
//! tests.

use anodizer_core::context::Context;
use anodizer_core::{Publisher, PublisherGroup, PublisherOutcome, SkipReason};
use anodizer_stage_publish::testing::{FakeOutcome, fake};
use anodizer_stage_publish::{DispatchOptions, dispatch};

#[test]
fn three_managers_middle_fails_siblings_still_run() {
    // A succeeds, B fails, C succeeds. All Manager. None required.
    let publishers: Vec<Box<dyn Publisher>> = vec![
        fake("a", PublisherGroup::Manager, false, FakeOutcome::Succeed),
        fake(
            "b",
            PublisherGroup::Manager,
            false,
            FakeOutcome::Fail("fake failure from 'b'".into()),
        ),
        fake("c", PublisherGroup::Manager, false, FakeOutcome::Succeed),
    ];

    let mut ctx = Context::test_fixture();
    let opts = DispatchOptions::default();
    let report = dispatch(&publishers, &mut ctx, &opts).expect("dispatch returns Ok");

    assert_eq!(report.results.len(), 3, "all three publishers ran");
    assert_eq!(report.results[0].name, "a");
    assert_eq!(report.results[1].name, "b");
    assert_eq!(report.results[2].name, "c");

    assert!(
        matches!(report.results[0].outcome, PublisherOutcome::Succeeded),
        "A: succeeded"
    );
    assert!(
        matches!(report.results[1].outcome, PublisherOutcome::Failed(_)),
        "B: failed"
    );
    assert!(
        matches!(report.results[2].outcome, PublisherOutcome::Succeeded),
        "C: succeeded"
    );

    assert!(
        !report.submitter_gated,
        "no Submitter configured, gate did not fire"
    );
    assert_eq!(
        report.required_failures(),
        0,
        "B is required=false; no required failures"
    );
}

#[test]
fn three_managers_middle_required_failure_gates_later_sibling() {
    // Same shape but B is required=true. Sibling isolation is asymmetric
    // across a REQUIRED failure: the sibling BEFORE it (A) still runs — the
    // gate wasn't closed yet when A dispatched — but the sibling AFTER it (C)
    // is now gated. A required failure in a one-way-door group (Manager here)
    // closes the gate, and no later one-way door may fire against an
    // incomplete release (F1). Contrast `three_managers_middle_fails_siblings_
    // still_run`, where B is OPTIONAL, the gate stays open, and C runs.
    let publishers: Vec<Box<dyn Publisher>> = vec![
        fake("a", PublisherGroup::Manager, false, FakeOutcome::Succeed),
        fake(
            "b",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Fail("fake failure from 'b'".into()),
        ),
        fake("c", PublisherGroup::Manager, false, FakeOutcome::Succeed),
    ];

    let mut ctx = Context::test_fixture();
    let opts = DispatchOptions::default();
    let report = dispatch(&publishers, &mut ctx, &opts).expect("dispatch returns Ok");

    assert_eq!(report.results.len(), 3);
    assert!(
        matches!(report.results[0].outcome, PublisherOutcome::Succeeded),
        "A: ran before the gate closed"
    );
    assert!(
        matches!(report.results[1].outcome, PublisherOutcome::Failed(_)),
        "B: required, failed — closes the gate"
    );
    assert!(
        matches!(
            report.results[2].outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        ),
        "C: a later Manager one-way door, gated by B's required failure, got {:?}",
        report.results[2].outcome
    );
    assert!(
        report.submitter_gated,
        "a required Manager failure must close the one-way-door gate"
    );
    assert_eq!(
        report.required_failures(),
        1,
        "B is required=true and failed"
    );
}
